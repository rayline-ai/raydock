//! `raydock claude-desktop`: Claude Code inside the Claude desktop app.
//!
//! Claude Desktop runs on the shared local process, which is not available
//! yet, so `start`, `status`, `validate`, `stop`, `install-service` and
//! `uninstall-service` fail. `off` and `uninstall` restore the Desktop profile
//! and deployment mode that were selected before Rayline ARC, from the recovery
//! record in `~/.rayline/claude-desktop/`.

mod desktop;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppAction {
    Start,
    Status,
    Validate,
    Stop,
    InstallService,
    UninstallService,
    /// Restore the previous Desktop profile and deployment mode.
    Off,
    Uninstall,
}

impl AppAction {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "start" => Self::Start,
            "status" => Self::Status,
            "validate" => Self::Validate,
            "stop" => Self::Stop,
            "install-service" => Self::InstallService,
            "uninstall-service" => Self::UninstallService,
            "off" => Self::Off,
            "uninstall" => Self::Uninstall,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppRequest {
    pub action: AppAction,
}

/// Create `dir` and narrow it to 0700, so no other user can rename or replace
/// the files in it whatever the umask.
fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `contents` readable only by the owner: into a new 0600 file beside
/// `path` (in a directory narrowed to 0700), fsynced, then renamed over it and
/// the directory fsynced. Whatever was at `path` (a loosened file, a planted
/// symlink) is replaced, never written through.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    private_dir(dir)?;
    let name = path
        .file_name()
        .map(OsStr::to_string_lossy)
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&tmp)?;
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    if let Err(error) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    // Make the rename itself durable.
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn off_instructions() -> &'static str {
    "\
To return Claude Desktop to your claude.ai subscription:
  1. In Claude Desktop, open the account menu at the bottom left (it shows
     \"Gateway\") and choose Sign out.
  2. On the sign-in screen, sign in with your Anthropic account instead of the
     configured gateway.
Your subscription data is kept. The imported gateway configuration stays
available under Developer > Configure Third-Party Inference... for next time.
"
}

pub async fn run(request: AppRequest) -> ExitCode {
    match run_inner(&request) {
        Ok(message) => {
            print!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("Error: {message}");
            ExitCode::from(1)
        }
    }
}

/// A cross-process lease on Desktop's profile, outside the Desktop state
/// directory. LOCK_NB makes a second `off` or `uninstall` fail immediately.
#[cfg(unix)]
fn lifecycle_lock(home: &Path) -> io::Result<fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let root = home.join(crate::DOT_CONFIG_DIR);
    private_dir(&root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("claude-desktop.lock"))?;
    // SAFETY: flock receives a live descriptor and does not retain pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other(
            "Another Rayline command is changing the Desktop lifecycle",
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn lifecycle_lock(_home: &Path) -> io::Result<fs::File> {
    Err(io::Error::other(
        "Claude Desktop lifecycle commands require macOS or Linux",
    ))
}

fn run_inner(request: &AppRequest) -> Result<String, String> {
    let unavailable = match request.action {
        AppAction::Start => "start",
        AppAction::Status => "status",
        AppAction::Validate => "validate",
        AppAction::Stop => "stop",
        AppAction::InstallService => "install-service",
        AppAction::UninstallService => "uninstall-service",
        AppAction::Off => return off(),
        AppAction::Uninstall => return uninstall(),
    };
    Err(format!(
        "`raydock claude-desktop {unavailable}` is not available yet"
    ))
}

fn restore() -> Result<bool, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let _lease = lifecycle_lock(&home).map_err(|error| error.to_string())?;
    desktop::restore(&home).map_err(|error| error.to_string())
}

fn off() -> Result<String, String> {
    if restore()? {
        Ok("Restored your previous Claude Desktop profile and deployment mode. You may need to sign in again.\n".to_owned())
    } else {
        Ok(off_instructions().to_owned())
    }
}

fn uninstall() -> Result<String, String> {
    let mut out = if restore()? {
        "Restored your previous Claude Desktop profile.\n".to_owned()
    } else {
        String::new()
    };
    out.push_str(
        "The saved Rayline ARC profile remains in Desktop. To remove the gateway \
         configuration, open Developer > Configure Third-Party Inference... in Desktop.\n\n",
    );
    out.push_str(off_instructions());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_private_narrows_an_existing_file_before_writing() {
        use std::os::unix::fs::PermissionsExt;
        let home =
            std::env::temp_dir().join(format!("rayline-claude-app-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        let path = home.join("secret");
        fs::write(&path, "old").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&path, b"new").expect("write");
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).expect("read"), "new");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn actions_parse() {
        for (text, action) in [
            ("start", AppAction::Start),
            ("status", AppAction::Status),
            ("validate", AppAction::Validate),
            ("stop", AppAction::Stop),
            ("install-service", AppAction::InstallService),
            ("uninstall-service", AppAction::UninstallService),
            ("off", AppAction::Off),
            ("uninstall", AppAction::Uninstall),
        ] {
            assert_eq!(AppAction::parse(text), Some(action));
        }
        assert_eq!(AppAction::parse("serve"), None);
        assert_eq!(AppAction::parse("bogus"), None);
    }

    #[test]
    fn local_process_actions_are_not_available_yet() {
        for (name, action) in [
            ("start", AppAction::Start),
            ("status", AppAction::Status),
            ("validate", AppAction::Validate),
            ("stop", AppAction::Stop),
            ("install-service", AppAction::InstallService),
            ("uninstall-service", AppAction::UninstallService),
        ] {
            let error = run_inner(&AppRequest { action }).unwrap_err();
            assert_eq!(
                error,
                format!("`raydock claude-desktop {name}` is not available yet")
            );
        }
    }
}
