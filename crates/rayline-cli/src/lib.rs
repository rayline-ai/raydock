use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

pub(crate) mod arc_release;
pub(crate) mod auth_callback_page;
pub mod claude;
pub(crate) mod claude_daemon;
mod cli;
pub mod codex;
pub mod codex_app;
pub mod config;
pub(crate) mod credentials;
pub(crate) mod detach;
pub mod harness;
pub mod hybrid;
pub(crate) mod hybrid_profile;
pub(crate) mod hybrid_tokenizer;
pub mod status;
mod top;
pub mod update;

pub const CLI_BIN: &str = "rayline";
/// The release asset name.
pub const CLI_ASSET: &str = "rayline";
pub const DISPLAY_NAME: &str = "Rayline CLI";
pub const CONFIG_DIR: &str = "rayline";
pub const DOT_CONFIG_DIR: &str = ".rayline";
pub const ROUTER_PROD_URL: &str = "https://api.rayline.ai";
/// The model a hosted harness launch gives its main agent when the user picks
/// none: ARC Sage, by its canonical public id (not the bare `arc-sage` alias,
/// which tracks the newest ARC line).
pub const ARC_SAGE_MODEL: &str = "arc-sage-1.0";
/// The model a hosted harness launch gives subagents and background tasks,
/// where the harness tells them apart from the main agent: ARC Swift.
pub const ARC_SWIFT_MODEL: &str = "arc-swift-1.0";
/// The virtual model of the local process: the hybrid execution core's only
/// model. It names no hosted model; the hosted router refuses it.
pub const VIRTUAL_ROUTER_MODEL: &str = "rayline-auto";
pub const UPDATE_BASE_URL: &str = "https://get.rayline.ai";
pub const INSTALLER_URL: &str = "https://get.rayline.ai/install.sh";
pub const CLAUDE_LAUNCHES_SUFFIX: &str = ".rayline/harness/claude-code/daemon-launches.json";
pub const RAYLINE_VERSION: &str = env!("RAYLINE_VERSION");

/// The env var the home directory comes from.
#[cfg(not(windows))]
const HOME_VAR: &str = "HOME";
/// The env var the home directory comes from.
#[cfg(windows)]
const HOME_VAR: &str = "USERPROFILE";

/// The home directory is unknown: its env var is unset or empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeError;

impl fmt::Display for HomeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{HOME_VAR}` is not set, so the home directory is unknown"
        )
    }
}

impl std::error::Error for HomeError {}

/// The user's home directory: `HOME` on unix, `USERPROFILE` on Windows.
pub(crate) fn home() -> Result<PathBuf, HomeError> {
    match env::var_os(HOME_VAR) {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home)),
        _ => Err(HomeError),
    }
}

// Production minisign public key(s) for release / self-update signature verification.
// The matching secret key lives only in the GitHub `release` environment secret
// MINISIGN_SECRET_KEY (see RELEASING-SIGNING.md). Rotation: add the next public key
// alongside the current one (verification accepts any listed key), ship a release, then
// retire the old key after users have updated.
pub const MINISIGN_PUBLIC_KEYS: &[&str] = &[
    "RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1", // rayline production signing key (2026-06)
];

pub async fn run() -> ExitCode {
    let original_argv = env::args_os().collect::<Vec<_>>();
    run_argv(&original_argv).await
}

/// Parses the command line, loads the config once and runs the command. A
/// usage error exits 2 and a config error exits 1, both before anything runs.
pub async fn run_argv(original_argv: &[OsString]) -> ExitCode {
    let (env, command) = match cli::parse(original_argv) {
        Ok(cli::Parsed::Help(help)) => {
            print!("{help}");
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Version) => {
            println!("{CLI_BIN} {RAYLINE_VERSION}");
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::LocalProcess(args)) => {
            return match rayline_daemon::run(args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(cli::Parsed::Command { env, command }) => (env, command),
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let (mode, harness_protocol) = match &command {
        cli::Command::Run(run) => (
            match &run.config {
                cli::ConfigFlag::Default => config::Mode::Cloud,
                cli::ConfigFlag::Local => config::Mode::Local,
                cli::ConfigFlag::File(file) => config::Mode::File(file),
            },
            Some(run.launch.protocol()),
        ),
        cli::Command::Local(cli::Local::Start { config: Some(file) }) => {
            (config::Mode::LocalFile(file), None)
        }
        cli::Command::Local(cli::Local::Start { config: None }) => (config::Mode::Local, None),
        cli::Command::Auth(_)
        | cli::Command::Local(cli::Local::Status | cli::Local::Stop)
        | cli::Command::Top { .. }
        | cli::Command::Update(_) => (config::Mode::Cloud, None),
    };
    let settings = match config::load(&config::LoadOptions {
        mode,
        env: env.as_deref(),
        harness_protocol,
    }) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    let hosted = status::hosted_environment(&settings);
    match command {
        cli::Command::Auth(auth) => run_auth(auth, hosted).await,
        cli::Command::Run(run) if settings.local => run_local(run, &settings).await,
        cli::Command::Local(cli::Local::Start { .. }) => hybrid::start(&settings).await,
        cli::Command::Local(cli::Local::Status) => hybrid::status().await,
        cli::Command::Local(cli::Local::Stop) => hybrid::stop().await,
        cli::Command::Run(run) => run_cloud(run.launch, hosted).await,
        cli::Command::Top { json } => {
            print_or_fail(top::render_top(&top::TopRequest { json }).await, "Error")
        }
        cli::Command::Update(request) => {
            match update::run(&request, settings.update.channel).await {
                Ok(result) => {
                    if result.stderr {
                        eprint!("{}", result.message);
                    } else {
                        print!("{}", result.message);
                    }
                    ExitCode::from(result.exit_code)
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

async fn run_auth(auth: cli::Auth, hosted: status::HostedEnvironment) -> ExitCode {
    match auth {
        cli::Auth::Login { no_browser, paste } => {
            let request = status::AuthLoginRequest {
                hosted,
                no_browser,
                paste,
            };
            match status::auth_login(&request).await {
                Ok(message) => write_auth_message(&message, "login"),
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        cli::Auth::Logout => {
            let request = status::AuthLogoutRequest { hosted };
            match status::logout(&request).await {
                Ok(message) => write_auth_message(&message, "logout"),
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        cli::Auth::Status => {
            let request = status::StatusRequest { hosted };
            print_or_fail(status::render_status(&request), "Error")
        }
        cli::Auth::Token => {
            let request = status::AuthTokenRequest { hosted };
            match status::auth_token(&request).await {
                Ok(value) => {
                    println!("{}", terminal_output_text(&value));
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

fn write_auth_message(message: &str, action: &str) -> ExitCode {
    match status::write_auth_message(message) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: failed to write {action} output: {error}");
            ExitCode::from(1)
        }
    }
}

fn print_or_fail<E: std::fmt::Display>(result: Result<String, E>, prefix: &str) -> ExitCode {
    match result {
        Ok(message) => {
            print!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{prefix}: {error}");
            ExitCode::from(1)
        }
    }
}

/// A local-mode run on the local process. The kept options it does not run
/// are refused here, in one place.
async fn run_local(run: cli::Run, settings: &config::Settings) -> ExitCode {
    if let Some(option) = run.not_in_local_mode() {
        eprintln!("Error: {option} is not available in local mode");
        return ExitCode::from(1);
    }
    let (name, args) = match run.launch {
        cli::Launch::Claude { args, .. } => ("claude", args),
        cli::Launch::Codex { args, .. } => ("codex", args),
        cli::Launch::Harness { harness, args } => (harness.name(), args),
        cli::Launch::CodexApp { .. } | cli::Launch::CodexConfigure(_) => {
            unreachable!("not_in_local_mode refuses these launches")
        }
    };
    hybrid::run_local(settings, name, &args).await
}

/// A cloud-mode run through the hosted router.
async fn run_cloud(launch: cli::Launch, hosted: status::HostedEnvironment) -> ExitCode {
    match launch {
        cli::Launch::Claude { isolated, args } => {
            exec_claude(claude::RunRequest {
                hosted,
                args,
                isolated,
            })
            .await
        }
        cli::Launch::Codex { options, args } => {
            codex::run(codex::RunRequest {
                model: options.model,
                codex_args: args,
                hosted,
            })
            .await
        }
        cli::Launch::CodexApp { options, args } => {
            codex_app::run(codex_app::AppRunRequest {
                model: options.model,
                codex_args: args,
                hosted,
            })
            .await
        }
        cli::Launch::CodexConfigure(options) => {
            let request = codex::ConfigureRequest {
                model: options.model,
                hosted,
            };
            print_or_fail(
                codex::configure(&request).await,
                "Error: failed to configure Codex profile",
            )
        }
        cli::Launch::Harness { harness, args } => {
            harness::run(harness::RunRequest {
                harness,
                hosted,
                args,
            })
            .await
        }
    }
}

/// The exit code to pass on for a harness that exited with `status`: its own
/// code when it fits in 0..=255, else 1. A harness killed by a signal has no
/// code, and a Windows code above 255 (such as a crash status) is not
/// truncated to its low byte.
pub(crate) fn child_exit_code(status: std::process::ExitStatus) -> ExitCode {
    ExitCode::from(exit_code_byte(status.code()))
}

fn exit_code_byte(code: Option<i32>) -> u8 {
    code.and_then(|code| u8::try_from(code).ok()).unwrap_or(1)
}

fn terminal_output_text(value: &str) -> String {
    value.chars().collect()
}

async fn exec_claude(request: crate::claude::RunRequest) -> ExitCode {
    let print_mode = crate::claude::print_mode_flag(&request.args).is_some();
    let mut command = match crate::claude::run_command(&request).await {
        Ok(command) => command,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };

    // Separate Rayline's setup output from Claude Code's own (the interactive
    // banner, or the `-p` response). In print mode Claude emits nothing until the
    // model answers — which reads as a hang — so note that we're waiting, then a
    // rule below it marks the handoff. All on stderr, so piped `-p` stdout stays
    // clean.
    eprintln!();
    if print_mode {
        eprintln!("Prompt submitted to Claude Code — waiting for the response");
    }
    eprintln!("{}", "─".repeat(72));

    exec_or_status(&mut command)
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    eprintln!("rayline: failed to exec claude: {error}");
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => crate::child_exit_code(status),
        Err(error) => {
            eprintln!("rayline: failed to run claude: {error}");
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_harness_exit_code_outside_a_byte_is_1() {
        assert_eq!(exit_code_byte(Some(0)), 0);
        assert_eq!(exit_code_byte(Some(3)), 3);
        assert_eq!(exit_code_byte(Some(255)), 255);
        assert_eq!(exit_code_byte(Some(256)), 1);
        assert_eq!(exit_code_byte(Some(257)), 1);
        assert_eq!(exit_code_byte(Some(-1)), 1);
        // STATUS_ACCESS_VIOLATION, whose low byte is 5.
        assert_eq!(exit_code_byte(Some(0xC000_0005_u32 as i32)), 1);
        assert_eq!(exit_code_byte(None), 1);
    }

    #[test]
    fn home_comes_only_from_its_env_var() {
        let saved = env::var_os(HOME_VAR);
        let dir = env::temp_dir().join("rayline-home-var");
        // SAFETY: the workspace tests run with --test-threads=1; restored below.
        unsafe { env::remove_var(HOME_VAR) };
        let unset = home();
        unsafe { env::set_var(HOME_VAR, "") };
        let empty = home();
        unsafe { env::set_var(HOME_VAR, &dir) };
        let set = home();
        match saved {
            Some(value) => unsafe { env::set_var(HOME_VAR, value) },
            None => unsafe { env::remove_var(HOME_VAR) },
        }

        assert_eq!(unset, Err(HomeError));
        assert_eq!(empty, Err(HomeError));
        assert_eq!(set, Ok(dir));
        assert_eq!(
            HomeError.to_string(),
            format!("`{HOME_VAR}` is not set, so the home directory is unknown")
        );
    }
}
