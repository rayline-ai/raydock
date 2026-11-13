//! Keep the canonical command and the legacy alias on one update path.

use std::path::{Path, PathBuf};

use crate::update::{self, UpdateError};

pub(crate) fn primary_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}{}",
        crate::CLI_BIN,
        std::env::consts::EXE_SUFFIX
    ))
}

/// The 0.2.14 updater only replaces `rayline`. On its next invocation the new
/// payload finishes the migration before dispatching any user command. Managed
/// installs must be migrated by their package manager, never modified in place.
pub(crate) fn migrate_legacy_payload() -> Result<Option<PathBuf>, UpdateError> {
    let running = std::env::current_exe().map_err(|error| {
        UpdateError::Install(format!("failed to locate the running executable: {error}"))
    })?;
    // Linux marks an unlinked running executable with this suffix. Another
    // first invocation may replace either the legacy payload or the primary
    // while our forwarded process starts. Accept only the completed migration
    // layout, never an arbitrary sibling.
    #[cfg(target_os = "linux")]
    if matches!(
        running.file_name().and_then(|name| name.to_str()),
        Some("rayline (deleted)" | "raydock (deleted)")
    ) {
        let primary = primary_path(&running);
        let alias = running.with_file_name("rayline");
        if std::fs::canonicalize(&alias).map_err(update::io_failed("resolve", &alias))? != primary {
            return Err(UpdateError::Install(
                "Concurrent Raydock migration did not leave a valid rayline alias. Re-run the Raydock installer.".to_owned(),
            ));
        }
        return Ok(Some(primary));
    }
    let executable =
        std::fs::canonicalize(&running).map_err(update::io_failed("resolve", &running))?;
    if executable.file_name().and_then(|name| name.to_str())
        != Some(if cfg!(windows) {
            "rayline.exe"
        } else {
            "rayline"
        })
    {
        return Ok(None);
    }
    if update::is_app_bundle_managed(&executable) || update::is_homebrew_managed(&executable)? {
        return Err(UpdateError::Install(
            "This managed installation still uses the old command layout. Update Raydock through its app or Homebrew package.".to_owned(),
        ));
    }
    let primary = primary_path(&executable);
    // Concurrent first invocations take turns on the legacy payload, and only
    // the first one replaces it. macOS fails an open through a symlink while a
    // rename replaces that symlink, so the alias must be replaced only once.
    #[cfg(unix)]
    let _payload = {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::AsRawFd;
        let payload =
            std::fs::File::open(&executable).map_err(update::io_failed("open", &executable))?;
        // SAFETY: flock receives a live descriptor and does not retain pointers.
        if unsafe { libc::flock(payload.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(update::io_failed("lock", &executable)(
                std::io::Error::last_os_error(),
            ));
        }
        let inspect = || update::io_failed("inspect", &executable);
        if std::fs::symlink_metadata(&executable)
            .map_err(inspect())?
            .ino()
            != payload.metadata().map_err(inspect())?.ino()
        {
            return Ok(Some(primary));
        }
        payload
    };
    update::replace_binary(&executable, &primary)
        .and_then(|()| install_alias(&primary))
        .map_err(|error| match error {
            UpdateError::InUse(message) => UpdateError::InUse(format!(
                "{message}. Another process holds the file; run the command again once it has closed it."
            )),
            error => UpdateError::Install(format!(
                "{error}. Re-run the Raydock installer with write access to the directory containing {}, or install into a user-writable directory and update PATH.",
                executable.display()
            )),
        })?;
    eprintln!("Use `raydock`; `rayline` remains an alias.");
    Ok(Some(primary))
}

pub(crate) fn install_alias(primary: &Path) -> Result<(), UpdateError> {
    let alias = primary.with_file_name(if cfg!(windows) {
        "rayline.exe"
    } else {
        "rayline"
    });
    let staged = update::staged_install_path(&alias)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(crate::CLI_BIN, &staged)
        .map_err(update::io_failed("create the alias", &staged))?;
    #[cfg(windows)]
    std::fs::write(
        &staged,
        include_bytes!(concat!(env!("OUT_DIR"), "/rayline-alias.exe")),
    )
    .map_err(update::io_failed("write the alias", &staged))?;
    let result = update::move_into_place(&staged, &alias);
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

pub(crate) fn forward(primary: &Path) -> std::process::ExitCode {
    let mut command = std::process::Command::new(primary);
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("rayline: could not start Raydock: {error}");
        std::process::ExitCode::FAILURE
    }
    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("rayline: could not start Raydock: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
