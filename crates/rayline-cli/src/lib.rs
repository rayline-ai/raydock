mod command_alias;
use std::env;
use std::ffi::OsString;
use std::process::{Command, ExitCode};

pub(crate) mod auth_callback_page;
pub mod claude;
pub mod claude_app;
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

pub const CLI_BIN: &str = "raydock";
/// Immutable download identifier; existing updaters verify these filenames.
pub const CLI_ASSET: &str = "rayline";
pub const DAEMON_BIN: &str = "rld";
pub const DISPLAY_NAME: &str = "Raydock";
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
/// The virtual model of the routers that run on this machine: the local
/// router reads it as "route by `routes.main`", and it is the hybrid execution
/// core's only model. It names no hosted model; the hosted router refuses it.
pub const VIRTUAL_ROUTER_MODEL: &str = "rayline-auto";
pub const UPDATE_BASE_URL: &str = "https://get.rayline.ai";
pub const INSTALLER_URL: &str = "https://get.rayline.ai/install.sh";
pub const CLAUDE_LAUNCHES_SUFFIX: &str = ".rayline/harness/claude-code/daemon-launches.json";
pub const RAYLINE_VERSION: &str = env!("RAYLINE_VERSION");

// Production minisign public key(s) for release / self-update signature verification.
// The matching secret key lives only in the GitHub `release` environment secret
// MINISIGN_SECRET_KEY (see RELEASING-SIGNING.md). Rotation: add the next public key
// alongside the current one (verification accepts any listed key), ship a release, then
// retire the old key after users have updated.
pub const MINISIGN_PUBLIC_KEYS: &[&str] = &[
    "RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1", // rayline production signing key (2026-06)
];

pub async fn run() -> ExitCode {
    match command_alias::migrate_legacy_payload() {
        Ok(Some(primary)) => return command_alias::forward(&primary),
        Ok(None) => {}
        Err(error) => {
            eprintln!("Raydock command migration failed: {error}");
            return ExitCode::FAILURE;
        }
    }
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
            println!("raydock {RAYLINE_VERSION}");
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Hybrid(args)) => return hybrid::run(args).await,
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
        cli::Command::Auth(_) | cli::Command::Top { .. } | cli::Command::Update(_) => {
            (config::Mode::Cloud, None)
        }
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
    let root_env_explicit = env.is_some();
    let env_name = settings.env.clone();
    match command {
        cli::Command::Auth(auth) => run_auth(auth, env_name, root_env_explicit).await,
        cli::Command::Run(run) if settings.local => run_local(run.launch).await,
        cli::Command::Run(run) => run_cloud(run.launch, env_name, root_env_explicit).await,
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

async fn run_auth(auth: cli::Auth, env_name: Option<String>, root_env_explicit: bool) -> ExitCode {
    match auth {
        cli::Auth::Login { no_browser, paste } => {
            let request = status::AuthLoginRequest {
                env_name,
                root_env_explicit,
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
            let request = status::AuthLogoutRequest {
                env_name,
                root_env_explicit,
            };
            match status::logout(&request).await {
                Ok(message) => write_auth_message(&message, "logout"),
                Err(error) => {
                    eprintln!("Error: failed to update credentials: {error}");
                    ExitCode::from(1)
                }
            }
        }
        cli::Auth::Status => {
            let request = status::StatusRequest {
                env_name,
                auth_token: None,
                root_env_explicit,
            };
            print_or_fail(status::render_status(&request), "Error")
        }
        cli::Auth::Token => {
            let shown_env = status::resolve_env(env_name.as_deref(), None);
            let request = status::AuthTokenRequest {
                env_name,
                auth_token: None,
                root_env_explicit,
            };
            match status::resolve_auth_token(&request).await {
                Ok(status::AuthTokenOutcome::Available(value)) => {
                    println!("{}", terminal_output_text(&value));
                    ExitCode::SUCCESS
                }
                Ok(status::AuthTokenOutcome::NotLoggedIn) => {
                    eprintln!("Error: Not logged in to {shown_env}. Run: rayline auth login");
                    ExitCode::from(1)
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

/// A local-mode run: today's `hybrid run` path. The kept options it does not
/// run yet are refused here, in one place, until sub-PR 6b.
async fn run_local(launch: cli::Launch) -> ExitCode {
    if let Some(option) = launch.not_in_local_mode() {
        eprintln!("Error: {option} is not available in local mode yet");
        return ExitCode::from(1);
    }
    let (name, args) = match launch {
        cli::Launch::Claude { args, .. } => ("claude", args),
        cli::Launch::Codex { args, .. } => ("codex", args),
        cli::Launch::Harness { harness, args } => (harness.name(), args),
        cli::Launch::ClaudeDesktop(_)
        | cli::Launch::CodexApp { .. }
        | cli::Launch::CodexConfigure(_) => {
            unreachable!("not_in_local_mode refuses these launches")
        }
    };
    hybrid::run_local(name, &args).await
}

/// A cloud-mode run through the hosted router.
async fn run_cloud(
    launch: cli::Launch,
    env_name: Option<String>,
    root_env_explicit: bool,
) -> ExitCode {
    match launch {
        cli::Launch::Claude { isolated, args } => {
            exec_claude(claude::RunRequest {
                env_name,
                auth_token: None,
                args,
                isolated,
                root_env_explicit,
            })
            .await
        }
        cli::Launch::ClaudeDesktop(action) => {
            claude_app::run(claude_app::AppRequest { action }).await
        }
        cli::Launch::Codex { options, args } => {
            codex::run(codex::RunRequest {
                model: options.model,
                auth_mode: options.auth.unwrap_or(codex::CodexAuthMode::Auto),
                codex_args: args,
                root_env_explicit,
                env_name,
                auth_token: None,
            })
            .await
        }
        cli::Launch::CodexApp { options, args } => {
            codex_app::run(codex_app::AppRunRequest {
                model: options.model,
                auth_mode: options.auth.unwrap_or(codex::CodexAuthMode::Auto),
                codex_args: args,
                root_env_explicit,
                env_name,
                auth_token: None,
            })
            .await
        }
        cli::Launch::CodexConfigure(options) => {
            let request = codex::ConfigureRequest {
                model: options.model,
                auth_mode: options.auth.unwrap_or(codex::CodexAuthMode::Auto),
                env_name,
                auth_token: None,
                root_env_explicit,
            };
            print_or_fail(
                codex::configure(&request).await,
                "Error: failed to configure Codex profile",
            )
        }
        cli::Launch::Harness { harness, args } => {
            harness::run(harness::RunRequest {
                harness,
                env_name,
                auth_token: None,
                root_env_explicit,
                args,
            })
            .await
        }
    }
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
    eprintln!("raydock: failed to exec claude: {error}");
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("raydock: failed to run claude: {error}");
            ExitCode::from(127)
        }
    }
}
