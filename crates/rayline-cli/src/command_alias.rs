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
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    if executable.file_name().and_then(|name| name.to_str())
        != Some(if cfg!(windows) {
            "rayline.exe"
        } else {
            "rayline"
        })
    {
        return Ok(None);
    }
    if update::is_app_bundle_managed(&executable) || update::is_homebrew_managed(&executable) {
        return Err(UpdateError::Install(
            "This managed installation still uses the old command layout. Update Raydock through its app or Homebrew package.".to_owned(),
        ));
    }
    let primary = primary_path(&executable);
    update::replace_binary(&executable, &primary)
        .and_then(|()| install_alias(&primary))
        .map_err(|error| {
            UpdateError::Install(format!(
                "{error}. Re-run the Raydock installer with write access to the directory containing {}, or install into a user-writable directory and update PATH.",
                executable.display()
            ))
        })?;
    eprintln!("Rayline Local is now Raydock. Use `raydock`; `rayline` remains an alias.");
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
    std::os::unix::fs::symlink(crate::CLI_BIN, &staged)?;
    #[cfg(windows)]
    std::fs::write(
        &staged,
        include_bytes!(concat!(env!("OUT_DIR"), "/rayline-alias.exe")),
    )?;
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
