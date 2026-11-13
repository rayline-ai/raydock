mod command_alias;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

pub(crate) mod auth_callback_page;
pub mod claude;
pub mod claude_app;
pub(crate) mod claude_daemon;
pub mod codex;
pub mod codex_app;
pub mod config;
pub(crate) mod detach;
pub mod harness;
pub mod hybrid;
pub(crate) mod hybrid_profile;
pub(crate) mod hybrid_tokenizer;
pub mod status;
mod top;
pub mod update;
mod usage_error;

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

const ROOT_HELP: &str = "\
Usage: raydock [OPTIONS] COMMAND [ARGS]...

Options:
  --version    Show version
  --help       Show this message and exit

Commands:
  auth       Sign in to hosted Rayline
  status     Show current CLI auth status
  claude     Run Claude Code on ARC (Sage main agent, Swift subagents)
  claude-desktop  Run Claude Desktop's Code sessions on ARC
  codex      Run Codex CLI on ARC through the Rayline router
  opencode   Run opencode on ARC through the Rayline router
  pi         Run pi on ARC through the Rayline router
  omp        Run oh-my-pi (omp) on ARC through the Rayline router
  hermes     Run Hermes Agent on ARC through the Rayline router
  openclaw   Run OpenClaw on ARC through the Rayline router

Harness launches default to ARC: the ARC Sage your key lists for the main
agent and its ARC Swift for subagents and background tasks where the harness
has a separate role for them.
  hybrid     Start the ARC execution core or connect a harness to it
  top        Show live request metrics from the local process
  update     Check for or install a Raydock launcher update
";

const STATUS_HELP: &str = "\
Usage: raydock status

Show current CLI auth status.
";

const AUTH_HELP: &str = "\
Usage: raydock auth COMMAND

Commands:
  login    Sign in to Rayline
  logout   Revoke the stored Rayline session and router key
  status   Show current auth status
  token    Print a valid account bearer token
";

const AUTH_LOGIN_HELP: &str = "\
Usage: raydock auth login [OPTIONS]

Options:
  -b, --no-browser     Use device-code login instead of a local callback
  -p, --paste          Paste a browser callback URL manually
  --help               Show this message and exit
";

const AUTH_LOGOUT_HELP: &str = "\
Usage: raydock auth logout

Revoke the stored Rayline session and remove the stored router key.
";

const AUTH_STATUS_HELP: &str = "\
Usage: raydock auth status

Show current auth status.
";

const AUTH_TOKEN_HELP: &str = "\
Usage: raydock auth token

Print a valid account bearer token for scripts and integrations.
";

const CLAUDE_HELP: &str = "\
Usage: raydock claude [OPTIONS] [--] [CLAUDE_ARGS]...
       raydock claude run [OPTIONS] [--] [CLAUDE_ARGS]...

Run Claude Code on ARC through the Rayline router.

The main agent runs on the ARC Sage your key lists, subagents and background
tasks on its ARC Swift (CLAUDE_CODE_SUBAGENT_MODEL, forced on every subagent,
and ANTHROPIC_DEFAULT_HAIKU_MODEL, unless you set them).

Options:
  --isolated    Use a separate Claude config dir so this session can run
                beside other Claude Code background agents
  --help        Show this message and exit
";

const CLAUDE_APP_HELP: &str = "\
Usage: raydock claude-desktop [COMMAND] [OPTIONS]

Run Claude Code inside the Claude desktop app on ARC. Desktop runs on the
shared local process, which is not available yet: start, status, validate,
stop, install-service and uninstall-service fail until it is. off and
uninstall restore the Desktop profile and mode selected before Rayline ARC.

Commands:
  start              Configure and launch Desktop with ARC (default)
  status             Show the Desktop check
  validate           Check Desktop's applied configuration
  stop               Stop serving Desktop
  install-service    Serve Desktop at login
  uninstall-service  Remove the login service
  off                Restore the previous Desktop profile and mode
  uninstall          Restore the previous Desktop profile and mode, and show
                     how to remove the Rayline ARC gateway from Desktop

Options:
  --help             Show this message and exit
";

const CODEX_HELP: &str = "\
Usage: raydock codex [OPTIONS] [--] [CODEX_ARGS]...
       raydock codex app [OPTIONS] [WORKSPACE]
       raydock codex configure [OPTIONS]

Run Codex CLI on ARC through the Rayline router. Codex runs on its own
CODEX_HOME (~/.rayline/harness/codex), whose config.toml holds the Rayline
provider and your router key.

Options:
  --model <model>         Model to request (default: your key's ARC Sage)
  --auth <mode>           Codex auth source: auto|subscription|none
                          (default: auto, your Rayline router key).
                          subscription and none are not available yet.
  --subscription          Alias for --auth subscription
  --help                  Show this message and exit
";

const CODEX_APP_HELP: &str = "\
Usage: raydock codex app [OPTIONS] [WORKSPACE]

Launch the Codex desktop app on ARC through the Rayline router.

The desktop app-server ignores -c overrides and is single-instance, so this
writes the Rayline provider into an isolated CODEX_HOME
(~/.rayline/harness/codex-app) and launches the app there. If the app is already
running on a different config, you'll be prompted to restart it.

Options:
  --model <model>         Model to request (default: your key's ARC Sage)
  --auth <mode>           Codex auth source: auto|subscription|none
                          (default: auto, your Rayline router key).
                          subscription and none are not available yet.
  --subscription          Alias for --auth subscription
  --help                  Show this message and exit
";

const CODEX_CONFIGURE_HELP: &str = "\
Usage: raydock codex configure [OPTIONS]

Write a Codex profile at $CODEX_HOME/rayline.config.toml (or ~/.codex) that
reaches ARC through the Rayline router with your router key.

Options:
  --model <model>         Model to request (default: your key's ARC Sage)
  --base-url <url>        Rayline OpenAI Responses base URL
                          (default: the environment's router /v1)
  --auth <mode>           Codex auth source: auto|subscription|none
                          (default: auto, your Rayline router key).
                          subscription and none are not available yet.
  --subscription          Alias for --auth subscription
  --help                  Show this message and exit
";

const TOP_HELP: &str = "\
Usage: raydock top [--json]

Show live request metrics from the local process.

Options:
  --json    Print one snapshot as JSON
  --help    Show this message and exit
";

const UPDATE_HELP: &str = "\
Usage: raydock update [OPTIONS]

Options:
  --check              Check for an update without installing
  --version <version>  Install or check a specific version
  --force              Install even when already current
  --dry-run            Download and verify without replacing Raydock
";

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

pub async fn run_argv(original_argv: &[OsString]) -> ExitCode {
    if root_version_requested(original_argv) {
        println!("raydock {RAYLINE_VERSION}");
        return ExitCode::SUCCESS;
    }

    if let Some(help) = rayline_help_for_argv(original_argv) {
        print!("{help}");
        return ExitCode::SUCCESS;
    }

    match rayline_dispatch_for_argv(original_argv) {
        RaylineDispatch::Version => {
            println!("raydock {RAYLINE_VERSION}");
            ExitCode::SUCCESS
        }
        RaylineDispatch::Status(request) => match status::render_status(&request) {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::AuthLogin(request) => match status::auth_login(&request).await {
            Ok(message) => match status::write_auth_message(&message) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Error: failed to write login output: {error}");
                    ExitCode::from(1)
                }
            },
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::AuthToken(request) => {
            let env_name =
                status::resolve_env(request.env_name.as_deref(), dirs::home_dir().as_deref());
            match status::resolve_auth_token(&request).await {
                Ok(status::AuthTokenOutcome::Available(value)) => {
                    let output = terminal_output_text(&value);
                    println!("{output}");
                    ExitCode::SUCCESS
                }
                Ok(status::AuthTokenOutcome::NotLoggedIn) => {
                    eprintln!("Error: Not logged in to {env_name}. Run: raydock auth login");
                    ExitCode::from(1)
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::AuthLogout(request) => match status::logout(&request).await {
            Ok(message) => match status::write_auth_message(&message) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Error: failed to write logout output: {error}");
                    ExitCode::from(1)
                }
            },
            Err(error) => {
                eprintln!("Error: failed to update credentials: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::ClaudeRun(request) => exec_claude(request).await,
        RaylineDispatch::ClaudeApp(request) => claude_app::run(request).await,
        RaylineDispatch::CodexRun(request) => codex::run(request).await,
        RaylineDispatch::CodexApp(request) => codex_app::run(request).await,
        RaylineDispatch::Harness(request) => harness::run(request).await,
        RaylineDispatch::Hybrid(args) => hybrid::run(args).await,
        RaylineDispatch::CodexConfigure(request) => match codex::configure(&request).await {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: failed to configure Codex profile: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::Top(request) => match top::render_top(&request).await {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::Update(request) => match update::run(&request).await {
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
        },
        RaylineDispatch::Unavailable(command) => {
            eprintln!("raydock: `{command}` is not available in this public Raydock build");
            ExitCode::from(127)
        }
        RaylineDispatch::Usage(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

fn terminal_output_text(value: &str) -> String {
    value.chars().collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RaylineDispatch {
    Version,
    Status(status::StatusRequest),
    AuthLogin(status::AuthLoginRequest),
    AuthToken(status::AuthTokenRequest),
    AuthLogout(status::AuthLogoutRequest),
    ClaudeRun(claude::RunRequest),
    ClaudeApp(claude_app::AppRequest),
    CodexRun(codex::RunRequest),
    CodexApp(codex_app::AppRunRequest),
    CodexConfigure(codex::ConfigureRequest),
    Harness(harness::RunRequest),
    Hybrid(Vec<OsString>),
    Top(top::TopRequest),
    Update(update::UpdateRequest),
    /// A command that exists only in non-public builds; holds its words.
    Unavailable(String),
    /// Rejected arguments; holds the message to print.
    Usage(String),
}

pub fn rayline_dispatch_for_argv(original_argv: &[OsString]) -> RaylineDispatch {
    if root_version_requested(original_argv) {
        return RaylineDispatch::Version;
    }

    let taken = std::cell::Cell::new(0);
    let exhausted = std::cell::Cell::new(false);
    let args = usage_error::TrackedArgs::new(original_argv, &taken, &exhausted);
    parse_dispatch(args.peekable())
        .unwrap_or_else(|| usage_error::rejected_argv(original_argv, taken.get(), exhausted.get()))
}

/// Parses everything after the program name. `None` means the arguments were
/// rejected; the caller reports where parsing stopped.
fn parse_dispatch<'a, I>(mut args: std::iter::Peekable<I>) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut root_env = None;
    let mut root_auth_token = None;
    let mut root_env_explicit = false;

    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--" {
            return None;
        }
        if let Some((name, value)) = arg.split_once('=') {
            match name {
                "--env" => {
                    if !status::is_valid_root_env(value) {
                        return None;
                    }
                    root_env = Some(value.to_owned());
                    root_env_explicit = true;
                    continue;
                }
                "--auth-token" => {
                    root_auth_token = Some(value.to_owned());
                    continue;
                }
                _ => {}
            }
        }
        if is_value_option(arg) {
            if arg.contains('=') {
                continue;
            }
            let value = args.next()?;
            if arg == "--env" {
                let value = value.to_str()?;
                if !status::is_valid_root_env(value) {
                    return None;
                }
                root_env = Some(value.to_owned());
                root_env_explicit = true;
            } else if arg == "--auth-token" {
                root_auth_token = Some(value.to_str()?.to_owned());
            }
            continue;
        }
        if arg == "--version" || arg == "--help" {
            continue;
        }
        if arg.starts_with('-') {
            return None;
        }

        return match arg {
            "hybrid" if !root_env_explicit && root_auth_token.is_none() => {
                Some(RaylineDispatch::Hybrid(args.cloned().collect()))
            }
            "hybrid" => Some(RaylineDispatch::Usage(
                usage_error::HYBRID_ROOT_OPTIONS.to_owned(),
            )),
            "auth" => parse_auth_dispatch(args, root_env, root_auth_token, root_env_explicit),
            "claude-desktop" => parse_claude_app_request(args).map(RaylineDispatch::ClaudeApp),
            "claude" => parse_claude_dispatch(args, root_env, root_auth_token, root_env_explicit),
            "codex" => parse_codex_dispatch(args, root_env, root_auth_token, root_env_explicit),
            "status" => parse_status_request(args, root_env, root_auth_token, root_env_explicit)
                .map(RaylineDispatch::Status),
            "top" => parse_top_request(args).map(RaylineDispatch::Top),
            "update" => parse_update_request(args).map(RaylineDispatch::Update),
            command => harness::Harness::from_command(command).and_then(|harness| {
                parse_harness_request(harness, args, root_env, root_auth_token, root_env_explicit)
                    .map(RaylineDispatch::Harness)
            }),
        };
    }

    None
}

/// `raydock [--env E] <harness> [--env E] [--auth-token T] [--] [ARGS]...`.
/// Rayline options are read only before the first other arg; everything from
/// there (or after `--`) passes through to the harness, including `--help`.
fn parse_harness_request<'a, I>(
    harness: harness::Harness,
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<harness::RunRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut env_name = root_env;
    let mut auth_token = root_auth_token;
    let mut root_env_explicit = root_env_explicit;
    let mut harness_args = Vec::new();

    while let Some(arg) = args.next() {
        let Some(arg_str) = arg.to_str() else {
            harness_args.push(arg.clone());
            break;
        };
        if arg_str == "--" {
            break;
        }
        let (option, inline_value) = match arg_str.split_once('=') {
            Some((option, value)) => (option, Some(value)),
            None => (arg_str, None),
        };
        if !matches!(option, "--env" | "--auth-token") {
            harness_args.push(arg.clone());
            break;
        }
        let value = match inline_value {
            Some(value) => value,
            None => args.next()?.to_str()?,
        };
        if option == "--env" {
            if !status::is_valid_root_env(value) {
                return None;
            }
            env_name = Some(value.to_owned());
            root_env_explicit = true;
        } else {
            auth_token = Some(value.to_owned());
        }
    }
    harness_args.extend(args.cloned());

    Some(harness::RunRequest {
        harness,
        env_name,
        auth_token,
        root_env_explicit,
        args: harness_args,
    })
}

fn parse_auth_dispatch<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    match args.next()?.to_str()? {
        "login" => parse_auth_login_request(args, root_env, root_env_explicit)
            .map(RaylineDispatch::AuthLogin),
        "logout" => parse_auth_logout_request(args, root_env, root_env_explicit)
            .map(RaylineDispatch::AuthLogout),
        "status" => parse_status_request(args, root_env, root_auth_token, root_env_explicit)
            .map(RaylineDispatch::Status),
        "token" => parse_auth_token_request(args, root_env, root_auth_token, root_env_explicit)
            .map(RaylineDispatch::AuthToken),
        _ => None,
    }
}

fn parse_status_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<status::StatusRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let (env_name, auth_token, root_env_explicit) =
        parse_env_and_auth_options(args.by_ref(), root_env, root_auth_token, root_env_explicit)?;
    Some(status::StatusRequest {
        env_name,
        auth_token,
        root_env_explicit,
    })
}

fn parse_auth_token_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<status::AuthTokenRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let (env_name, auth_token, root_env_explicit) =
        parse_env_and_auth_options(args.by_ref(), root_env, root_auth_token, root_env_explicit)?;
    Some(status::AuthTokenRequest {
        env_name,
        auth_token,
        root_env_explicit,
    })
}

fn parse_auth_logout_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_env_explicit: bool,
) -> Option<status::AuthLogoutRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let (env_name, _, root_env_explicit) =
        parse_env_and_auth_options(args.by_ref(), root_env, None, root_env_explicit)?;
    Some(status::AuthLogoutRequest {
        env_name,
        root_env_explicit,
    })
}

fn parse_auth_login_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_env_explicit: bool,
) -> Option<status::AuthLoginRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut env_name = root_env;
    let mut root_env_explicit = root_env_explicit;
    let mut no_browser = false;
    let mut paste = false;

    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            if option == "--env" {
                if !status::is_valid_root_env(value) {
                    return None;
                }
                env_name = Some(value.to_owned());
                root_env_explicit = true;
                continue;
            }
            return None;
        }
        match arg {
            "--env" => {
                let value = args.next()?.to_str()?;
                if !status::is_valid_root_env(value) {
                    return None;
                }
                env_name = Some(value.to_owned());
                root_env_explicit = true;
            }
            "-b" | "--no-browser" => no_browser = true,
            "-p" | "--paste" => paste = true,
            _ => return None,
        }
    }

    Some(status::AuthLoginRequest {
        env_name,
        root_env_explicit,
        no_browser,
        paste,
    })
}

fn parse_env_and_auth_options<'a, I>(
    mut args: I,
    mut env_name: Option<String>,
    mut auth_token: Option<String>,
    mut root_env_explicit: bool,
) -> Option<(Option<String>, Option<String>, bool)>
where
    I: Iterator<Item = &'a OsString>,
{
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--env" => {
                    if !status::is_valid_root_env(value) {
                        return None;
                    }
                    env_name = Some(value.to_owned());
                    root_env_explicit = true;
                }
                "--auth-token" => auth_token = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--env" => {
                let value = args.next()?.to_str()?;
                if !status::is_valid_root_env(value) {
                    return None;
                }
                env_name = Some(value.to_owned());
                root_env_explicit = true;
            }
            "--auth-token" => auth_token = Some(args.next()?.to_str()?.to_owned()),
            _ => return None,
        }
    }

    Some((env_name, auth_token, root_env_explicit))
}

fn parse_claude_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<crate::claude::RunRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut env_name = root_env;
    let mut isolated = false;
    let mut claude_args = Vec::new();

    if args
        .peek()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| arg == "run")
    {
        let _ = args.next();
    } else if args
        .peek()
        .and_then(|arg| arg.to_str())
        .is_some_and(is_claude_management_subcommand)
    {
        return None;
    }

    while let Some(arg) = args.next() {
        let Some(arg_str) = arg.to_str() else {
            claude_args.push(arg.clone());
            continue;
        };

        if arg_str == "--help" {
            return None;
        }
        if arg_str == "--" {
            claude_args.extend(args.cloned());
            break;
        }
        if let Some(value) = arg_str.strip_prefix("--env=") {
            env_name = Some(value.to_owned());
            continue;
        }
        match arg_str {
            "--env" => {
                env_name = Some(args.next()?.to_str()?.to_owned());
                continue;
            }
            "--isolated" => {
                isolated = true;
                continue;
            }
            _ => {}
        }

        if claude_args.is_empty() && is_claude_management_subcommand(arg_str) {
            return None;
        }
        claude_args.push(arg.clone());
    }

    Some(crate::claude::RunRequest {
        env_name,
        auth_token: root_auth_token,
        args: claude_args,
        isolated,
        root_env_explicit,
    })
}

fn is_claude_management_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "login" | "logout" | "key" | "setup" | "hooks" | "telemetry"
    )
}

fn parse_claude_dispatch<'a, I>(
    args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    parse_claude_request(args, root_env, root_auth_token, root_env_explicit)
        .map(RaylineDispatch::ClaudeRun)
}

fn parse_claude_app_request<'a, I>(
    mut args: std::iter::Peekable<I>,
) -> Option<crate::claude_app::AppRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let action = match args.next() {
        Some(arg) => crate::claude_app::AppAction::parse(arg.to_str()?)?,
        None => crate::claude_app::AppAction::Start,
    };
    if args.next().is_some() {
        return None;
    }
    Some(crate::claude_app::AppRequest { action })
}

fn parse_codex_dispatch<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    match args.peek().and_then(|arg| arg.to_str()) {
        Some("configure") => {
            let _ = args.next();
            parse_codex_configure_request(args, root_env, root_auth_token, root_env_explicit)
                .map(RaylineDispatch::CodexConfigure)
        }
        Some("app") => {
            let _ = args.next();
            parse_codex_app_request(args, root_env, root_auth_token, root_env_explicit)
                .map(RaylineDispatch::CodexApp)
        }
        _ => parse_codex_request(args, root_env, root_auth_token, root_env_explicit)
            .map(RaylineDispatch::CodexRun),
    }
}

fn parse_codex_app_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    mut env_name: Option<String>,
    root_auth_token: Option<String>,
    mut root_env_explicit: bool,
) -> Option<crate::codex_app::AppRunRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model = None;
    let mut auth_mode = crate::codex::CodexAuthMode::Auto;
    let mut codex_args = Vec::new();

    while let Some(arg) = args.next() {
        let Some(arg_str) = arg.to_str() else {
            codex_args.push(arg.clone());
            codex_args.extend(args.cloned());
            break;
        };
        if arg_str == "--help" {
            return None;
        }
        if arg_str == "--" {
            codex_args.extend(args.cloned());
            break;
        }
        if let Some((option, value)) = arg_str.split_once('=') {
            match option {
                "--env" => {
                    env_name = Some(valid_env(value)?);
                    root_env_explicit = true;
                    continue;
                }
                "--model" => {
                    model = Some(value.to_owned());
                    continue;
                }
                "--auth" => {
                    auth_mode = crate::codex::CodexAuthMode::parse(value)?;
                    continue;
                }
                _ => {}
            }
        }
        match arg_str {
            "--env" => {
                env_name = Some(valid_env(args.next()?.to_str()?)?);
                root_env_explicit = true;
            }
            "--model" => model = Some(args.next()?.to_str()?.to_owned()),
            "--auth" => auth_mode = crate::codex::CodexAuthMode::parse(args.next()?.to_str()?)?,
            "--subscription" => auth_mode = crate::codex::CodexAuthMode::Subscription,
            _ => {
                codex_args.push(arg.clone());
                codex_args.extend(args.cloned());
                break;
            }
        }
    }

    Some(crate::codex_app::AppRunRequest {
        model,
        auth_mode,
        codex_args,
        root_env_explicit,
        env_name,
        auth_token: root_auth_token,
    })
}

fn parse_codex_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    mut env_name: Option<String>,
    root_auth_token: Option<String>,
    mut root_env_explicit: bool,
) -> Option<crate::codex::RunRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model = None;
    let mut auth_mode = crate::codex::CodexAuthMode::Auto;
    let mut codex_args = Vec::new();

    while let Some(arg) = args.next() {
        let Some(arg_str) = arg.to_str() else {
            codex_args.push(arg.clone());
            codex_args.extend(args.cloned());
            break;
        };
        if arg_str == "--help" {
            return None;
        }
        if arg_str == "--" {
            codex_args.extend(args.cloned());
            break;
        }
        if let Some((option, value)) = arg_str.split_once('=') {
            match option {
                "--env" => {
                    env_name = Some(valid_env(value)?);
                    root_env_explicit = true;
                    continue;
                }
                "--model" => {
                    model = Some(value.to_owned());
                    continue;
                }
                "--auth" => {
                    auth_mode = crate::codex::CodexAuthMode::parse(value)?;
                    continue;
                }
                _ => {}
            }
        }
        match arg_str {
            "--env" => {
                env_name = Some(valid_env(args.next()?.to_str()?)?);
                root_env_explicit = true;
            }
            "--model" => {
                model = Some(args.next()?.to_str()?.to_owned());
            }
            "--auth" => {
                auth_mode = crate::codex::CodexAuthMode::parse(args.next()?.to_str()?)?;
            }
            "--subscription" => {
                auth_mode = crate::codex::CodexAuthMode::Subscription;
            }
            _ => {
                codex_args.push(arg.clone());
                codex_args.extend(args.cloned());
                break;
            }
        }
    }

    Some(crate::codex::RunRequest {
        model,
        auth_mode,
        codex_args,
        root_env_explicit,
        env_name,
        auth_token: root_auth_token,
    })
}

fn valid_env(value: &str) -> Option<String> {
    status::is_valid_root_env(value).then(|| value.to_owned())
}

fn parse_codex_configure_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    env_name: Option<String>,
    auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<crate::codex::ConfigureRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model = None;
    let mut base_url = None;
    let mut auth_mode = crate::codex::CodexAuthMode::Auto;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--model" => {
                    model = Some(value.to_owned());
                    continue;
                }
                "--base-url" => {
                    base_url = Some(value.to_owned());
                    continue;
                }
                "--auth" => {
                    auth_mode = crate::codex::CodexAuthMode::parse(value)?;
                    continue;
                }
                _ => return None,
            }
        }
        match arg {
            "--model" => model = Some(args.next()?.to_str()?.to_owned()),
            "--base-url" => base_url = Some(args.next()?.to_str()?.to_owned()),
            "--auth" => auth_mode = crate::codex::CodexAuthMode::parse(args.next()?.to_str()?)?,
            "--subscription" => auth_mode = crate::codex::CodexAuthMode::Subscription,
            _ => return None,
        }
    }
    Some(crate::codex::ConfigureRequest {
        model,
        base_url,
        auth_mode,
        env_name,
        auth_token,
        root_env_explicit,
    })
}

/// `raydock top [--json]`.
fn parse_top_request<'a, I>(args: std::iter::Peekable<I>) -> Option<top::TopRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut json = false;
    for arg in args {
        match arg.to_str()? {
            "--json" => json = true,
            _ => return None,
        }
    }
    Some(top::TopRequest { json })
}

fn parse_update_request<'a, I>(mut args: std::iter::Peekable<I>) -> Option<update::UpdateRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut pinned_version = None;
    let mut base_url = None;
    let mut install_path = None;
    let mut force = false;
    let mut check_only = false;
    let mut dry_run = false;

    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--version" => pinned_version = Some(value.to_owned()),
                "--base-url" => base_url = Some(value.to_owned()),
                "--install-path" => install_path = Some(PathBuf::from(value)),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--version" => pinned_version = Some(args.next()?.to_str()?.to_owned()),
            "--base-url" => base_url = Some(args.next()?.to_str()?.to_owned()),
            "--install-path" => install_path = Some(PathBuf::from(args.next()?)),
            "--force" => force = true,
            "--check" => check_only = true,
            "--dry-run" => dry_run = true,
            _ => return None,
        }
    }

    if base_url.as_deref() == Some("")
        || install_path
            .as_ref()
            .is_some_and(|path: &PathBuf| path.as_os_str().is_empty())
    {
        return None;
    }
    Some(update::UpdateRequest {
        pinned_version,
        force,
        check_only,
        dry_run,
        base_url,
        install_path,
    })
}

fn root_version_requested(original_argv: &[OsString]) -> bool {
    let mut args = original_argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return false;
        };
        if arg == "--version" {
            return true;
        }
        if arg == "--" {
            return false;
        }
        if is_value_option(arg) {
            if !arg.contains('=') {
                args.next();
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return false;
    }
    false
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

fn rayline_help_for_argv(original_argv: &[OsString]) -> Option<&'static str> {
    help_for_command_path(&command_path_before_help(original_argv)?)
}

fn help_for_command_path(command: &[&str]) -> Option<&'static str> {
    match command {
        [] => Some(ROOT_HELP),
        ["auth"] => Some(AUTH_HELP),
        ["auth", "login"] => Some(AUTH_LOGIN_HELP),
        ["auth", "logout"] => Some(AUTH_LOGOUT_HELP),
        ["auth", "status"] => Some(AUTH_STATUS_HELP),
        ["auth", "token"] => Some(AUTH_TOKEN_HELP),
        ["claude"] | ["claude", "run"] => Some(CLAUDE_HELP),
        ["claude-desktop", ..] => Some(CLAUDE_APP_HELP),
        ["codex"] => Some(CODEX_HELP),
        ["codex", "app"] => Some(CODEX_APP_HELP),
        ["codex", "configure"] => Some(CODEX_CONFIGURE_HELP),
        ["status"] => Some(STATUS_HELP),
        ["top"] => Some(TOP_HELP),
        ["update"] => Some(UPDATE_HELP),
        _ => None,
    }
}

fn command_path_before_help(original_argv: &[OsString]) -> Option<Vec<&str>> {
    let mut args = original_argv.iter().skip(1).peekable();
    let mut command = Vec::new();

    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--" {
            return None;
        }
        if arg == "--help" {
            return Some(command);
        }
        if arg == "--version" {
            continue;
        }
        if is_value_option(arg) {
            if !arg.contains('=') {
                args.next()?;
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        command.push(arg);
    }

    if command.is_empty() || matches!(command.as_slice(), ["auth"]) {
        Some(command)
    } else {
        None
    }
}

fn is_value_option(arg: &str) -> bool {
    matches!(
        arg.split_once('=').map_or(arg, |(name, _)| name),
        "--env" | "--auth-token" | "--model" | "--base-url" | "--install-path"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn claude_run(args: &[&str]) -> crate::claude::RunRequest {
        let RaylineDispatch::ClaudeRun(request) = rayline_dispatch_for_argv(&argv(args)) else {
            panic!("expected ClaudeRun for {args:?}");
        };
        request
    }

    // ── Parser surface ────────────────────────────────────────────────────

    #[test]
    fn claude_help_lists_only_its_own_options() {
        let help =
            rayline_help_for_argv(&argv(&["rayline", "claude", "--help"])).expect("claude help");
        let options = help
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("--"))
            .filter_map(|line| line.split_whitespace().next())
            .collect::<Vec<_>>();
        assert_eq!(options, ["isolated", "help"]);
    }

    #[test]
    fn claude_passes_every_other_option_to_claude_code() {
        let request = claude_run(&["rayline", "claude", "--model", "opus", "--isolated", "-p"]);
        assert!(request.isolated);
        assert_eq!(request.args, argv(&["--model", "opus", "-p"]));
    }

    #[test]
    fn top_takes_only_json() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["raydock", "top"])),
            RaylineDispatch::Top(top::TopRequest { json: false })
        );
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["raydock", "top", "--json"])),
            RaylineDispatch::Top(top::TopRequest { json: true })
        );
        assert!(rayline_help_for_argv(&argv(&["raydock", "top"])).is_none());
        for args in [
            vec!["raydock", "top", "--bogus"],
            vec!["raydock", "top", "--json", "extra"],
        ] {
            assert!(
                matches!(
                    rayline_dispatch_for_argv(&argv(&args)),
                    RaylineDispatch::Usage(_)
                ),
                "{args:?} should be rejected"
            );
        }
    }

    #[test]
    fn router_and_local_are_not_commands() {
        for args in [
            vec!["raydock", "router", "start"],
            vec!["raydock", "router", "top"],
            vec!["raydock", "local", "models"],
        ] {
            assert!(
                !matches!(
                    rayline_dispatch_for_argv(&argv(&args)),
                    RaylineDispatch::Top(_) | RaylineDispatch::Harness(_)
                ),
                "{args:?} should be rejected"
            );
            assert!(rayline_help_for_argv(&argv(&args)).is_none());
        }
        assert_eq!(
            rayline_help_for_argv(&argv(&["raydock", "router", "--help"])),
            None
        );
    }

    #[test]
    fn root_version_is_public() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "--version"])),
            RaylineDispatch::Version
        );
    }

    #[test]
    fn public_help_surface_is_native() {
        for (args, needle) in [
            (&["rayline", "--help"][..], "Commands:"),
            (&["rayline", "update", "--help"][..], "raydock update"),
            (&["rayline", "top", "--help"][..], "raydock top"),
        ] {
            let help = rayline_help_for_argv(&argv(args)).expect("native help");
            assert!(help.contains(needle), "{args:?} should mention {needle:?}");
            assert!(
                !help.contains("--env"),
                "{args:?} should not expose hidden env selection"
            );
        }
    }

    #[test]
    fn public_parser_accepts_hosted_claude() {
        let dispatch = rayline_dispatch_for_argv(&argv(&[
            "rayline", "--env", "foo", "claude", "--", "--debug",
        ]));

        let RaylineDispatch::ClaudeRun(request) = dispatch else {
            panic!("expected ClaudeRun");
        };
        assert_eq!(request.env_name.as_deref(), Some("foo"));
        assert_eq!(
            request
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["--debug"]
        );
    }

    #[test]
    fn public_parser_accepts_auth_status_and_update() {
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "login", "--no-browser"])),
            RaylineDispatch::AuthLogin(status::AuthLoginRequest {
                no_browser: true,
                ..
            })
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "logout"])),
            RaylineDispatch::AuthLogout(_)
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "status"])),
            RaylineDispatch::Status(_)
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "token"])),
            RaylineDispatch::AuthToken(_)
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "status"])),
            RaylineDispatch::Status(_)
        ));
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "update", "--check", "--dry-run"])),
            RaylineDispatch::Update(update::UpdateRequest {
                pinned_version: None,
                force: false,
                check_only: true,
                dry_run: true,
                base_url: None,
                install_path: None,
            })
        );
    }

    #[test]
    fn update_takes_hidden_release_testing_options_and_no_channel() {
        let expected = RaylineDispatch::Update(update::UpdateRequest {
            pinned_version: Some("1.2.3".to_owned()),
            force: false,
            check_only: false,
            dry_run: true,
            base_url: Some("http://127.0.0.1:9/mirror".to_owned()),
            install_path: Some(PathBuf::from("/tmp/bin/raydock")),
        });
        for options in [
            &[
                "--base-url",
                "http://127.0.0.1:9/mirror",
                "--install-path",
                "/tmp/bin/raydock",
            ][..],
            &[
                "--base-url=http://127.0.0.1:9/mirror",
                "--install-path=/tmp/bin/raydock",
            ],
        ] {
            let mut args = vec!["rayline", "update"];
            args.extend_from_slice(options);
            args.extend(["--dry-run", "--version", "1.2.3"]);
            assert_eq!(rayline_dispatch_for_argv(&argv(&args)), expected);
        }
        for rejected in [
            &["rayline", "update", "--channel", "dev"][..],
            &["rayline", "update", "--channel=dev"],
            &["rayline", "update", "--base-url="],
            &["rayline", "update", "--install-path", ""],
        ] {
            assert!(
                matches!(
                    rayline_dispatch_for_argv(&argv(rejected)),
                    RaylineDispatch::Usage(_)
                ),
                "{rejected:?}"
            );
        }
        assert_eq!(
            rayline_help_for_argv(&argv(&[
                "rayline",
                "update",
                "--install-path",
                "/tmp/bin/raydock",
                "--help"
            ])),
            Some(UPDATE_HELP)
        );
        for hidden in ["--channel", "--base-url", "--install-path"] {
            assert!(!UPDATE_HELP.contains(hidden), "{hidden}");
        }
    }

    #[test]
    fn public_parser_accepts_claude_run_alias() {
        let dispatch = rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "claude",
            "run",
            "--isolated",
            "--",
            "--debug",
        ]));

        let RaylineDispatch::ClaudeRun(request) = dispatch else {
            panic!("expected ClaudeRun");
        };
        assert!(request.isolated);
        assert_eq!(
            request
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["--debug"]
        );
    }

    #[test]
    fn parser_rejects_invalid_hidden_env_names() {
        for env_name in ["", "foo.bar", "foo/bar", "foo bar"] {
            assert!(
                matches!(
                    rayline_dispatch_for_argv(&argv(&["rayline", "--env", env_name, "status"])),
                    RaylineDispatch::Usage(_)
                ),
                "{env_name:?} should be rejected"
            );
        }
    }

    #[test]
    fn public_parser_rejects_private_command_groups() {
        for args in [
            &["rayline", "internal", "status"][..],
            &["rayline", "install-cli"][..],
            &["rayline", "hook-send", "Stop"][..],
            &["rayline", "agent", "run"][..],
            &["rayline", "chat"][..],
            &["rayline", "claude", "login"][..],
            &["rayline", "claude", "hooks", "status"][..],
            &["rayline", "claude", "telemetry", "status"][..],
            &["rayline", "claude", "setup"][..],
            &["rayline", "claude", "run", "setup"][..],
        ] {
            assert!(
                matches!(
                    rayline_dispatch_for_argv(&argv(args)),
                    RaylineDispatch::Unavailable(_)
                ),
                "{args:?} should be unavailable"
            );
        }
    }

    #[test]
    fn codex_dispatch_defaults_to_no_model_and_passes_args() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "codex", "--", "exec", "hello"])) {
            RaylineDispatch::CodexRun(request) => {
                // No `--model`: the launch asks for the key's main model.
                assert_eq!(request.model, None);
                assert_eq!(request.auth_mode, crate::codex::CodexAuthMode::Auto);
                assert_eq!(request.codex_args, argv(&["exec", "hello"]));
            }
            other => panic!("expected CodexRun, got {other:?}"),
        }
    }

    #[test]
    fn codex_dispatch_accepts_explicit_real_model() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline", "codex", "--model", "gpt-5.5", "--", "exec", "hi",
        ])) {
            RaylineDispatch::CodexRun(request) => {
                assert_eq!(request.model.as_deref(), Some("gpt-5.5"));
                assert_eq!(request.codex_args, argv(&["exec", "hi"]));
            }
            other => panic!("expected CodexRun, got {other:?}"),
        }
    }

    #[test]
    fn codex_dispatch_accepts_subscription_auth() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "codex",
            "--subscription",
            "--",
            "exec",
            "hello",
        ])) {
            RaylineDispatch::CodexRun(request) => {
                assert_eq!(request.auth_mode, crate::codex::CodexAuthMode::Subscription);
                assert_eq!(request.codex_args, argv(&["exec", "hello"]));
            }
            other => panic!("expected CodexRun, got {other:?}"),
        }
    }

    #[test]
    fn claude_app_dispatch_defaults_to_start() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "claude-desktop"])) {
            RaylineDispatch::ClaudeApp(request) => {
                assert_eq!(request.action, crate::claude_app::AppAction::Start);
            }
            other => panic!("expected ClaudeApp, got {other:?}"),
        }
    }

    #[test]
    fn claude_app_dispatch_parses_subcommands() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "--env",
            "dev",
            "claude-desktop",
            "status",
        ])) {
            RaylineDispatch::ClaudeApp(request) => {
                assert_eq!(request.action, crate::claude_app::AppAction::Status);
            }
            other => panic!("expected ClaudeApp, got {other:?}"),
        }
        match rayline_dispatch_for_argv(&argv(&["rayline", "claude-desktop", "validate"])) {
            RaylineDispatch::ClaudeApp(request) => {
                assert_eq!(request.action, crate::claude_app::AppAction::Validate);
            }
            other => panic!("expected ClaudeApp, got {other:?}"),
        }
    }

    #[test]
    fn claude_app_dispatch_rejects_unknown_input() {
        for args in [
            &["rayline", "claude-desktop", "bogus"][..],
            &["rayline", "claude-desktop", "start", "stop"][..],
            &["rayline", "claude-desktop", "--bogus"][..],
            &["rayline", "claude-desktop", "off", "--bogus"][..],
        ] {
            assert!(
                matches!(
                    rayline_dispatch_for_argv(&argv(args)),
                    RaylineDispatch::Usage(_)
                ),
                "{args:?}"
            );
        }
    }

    #[test]
    fn claude_without_app_still_runs_claude_code() {
        // `raydock claude app` stays Claude Code's: `app` is a prompt word.
        match rayline_dispatch_for_argv(&argv(&["rayline", "claude", "app"])) {
            RaylineDispatch::ClaudeRun(request) => {
                assert_eq!(request.args, argv(&["app"]));
            }
            other => panic!("expected ClaudeRun, got {other:?}"),
        }
    }

    #[test]
    fn claude_app_help_is_routed() {
        for args in [
            &["rayline", "claude-desktop", "--help"][..],
            &["rayline", "claude-desktop", "status", "--help"][..],
        ] {
            assert_eq!(rayline_help_for_argv(&argv(args)), Some(CLAUDE_APP_HELP));
        }
        assert_eq!(
            rayline_help_for_argv(&argv(&["rayline", "claude", "--help"])),
            Some(CLAUDE_HELP)
        );
        // Without --help the bare command runs; it is not a help group.
        assert_eq!(
            rayline_help_for_argv(&argv(&["rayline", "claude-desktop"])),
            None
        );
        assert_eq!(
            rayline_help_for_argv(&argv(&["rayline", "claude-desktop", "status"])),
            None
        );
    }

    #[test]
    fn codex_app_dispatch_accepts_flags_and_workspace() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "codex",
            "app",
            "--model",
            "gpt-5.5",
            "--subscription",
            "/work/space",
        ])) {
            RaylineDispatch::CodexApp(request) => {
                assert_eq!(request.auth_mode, crate::codex::CodexAuthMode::Subscription);
                assert_eq!(request.model.as_deref(), Some("gpt-5.5"));
                // The workspace path is passed through to `codex app`.
                assert_eq!(request.codex_args, argv(&["/work/space"]));
            }
            other => panic!("expected CodexApp, got {other:?}"),
        }
    }

    #[test]
    fn codex_app_dispatch_defaults_and_passes_workspace() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "codex", "app", "."])) {
            RaylineDispatch::CodexApp(request) => {
                assert_eq!(request.model, None);
                assert_eq!(request.auth_mode, crate::codex::CodexAuthMode::Auto);
                assert_eq!(request.codex_args, argv(&["."]));
            }
            other => panic!("expected CodexApp, got {other:?}"),
        }
    }

    #[test]
    fn codex_configure_dispatch_accepts_model_and_base_url() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "codex",
            "configure",
            "--model",
            "gpt-5.5",
            "--base-url",
            "http://127.0.0.1:29999/v1",
        ])) {
            RaylineDispatch::CodexConfigure(request) => {
                assert_eq!(request.model.as_deref(), Some("gpt-5.5"));
                assert_eq!(request.auth_mode, crate::codex::CodexAuthMode::Auto);
                assert_eq!(
                    request.base_url.as_deref(),
                    Some("http://127.0.0.1:29999/v1")
                );
            }
            other => panic!("expected CodexConfigure, got {other:?}"),
        }
    }

    fn harness_run(args: &[&str]) -> crate::harness::RunRequest {
        match rayline_dispatch_for_argv(&argv(args)) {
            RaylineDispatch::Harness(request) => request,
            other => panic!("expected Harness, got {other:?}"),
        }
    }

    #[test]
    fn harness_commands_dispatch_and_pass_args_through() {
        use crate::harness::Harness;
        for harness in Harness::ALL {
            let request = harness_run(&["rayline", harness.name()]);
            assert_eq!(request.harness, harness);
            assert_eq!(request.env_name, None);
            assert!(request.args.is_empty());
        }

        let request = harness_run(&["rayline", "--env", "dev", "pi", "-p", "--env", "x"]);
        assert_eq!(request.harness, Harness::Pi);
        assert_eq!(request.env_name.as_deref(), Some("dev"));
        assert!(request.root_env_explicit);
        assert_eq!(request.args, argv(&["-p", "--env", "x"]));

        let request = harness_run(&[
            "rayline",
            "opencode",
            "--env=dev",
            "--auth-token",
            "t",
            "run",
            "hi",
        ]);
        assert_eq!(request.env_name.as_deref(), Some("dev"));
        assert_eq!(request.auth_token.as_deref(), Some("t"));
        assert_eq!(request.args, argv(&["run", "hi"]));

        let request = harness_run(&["rayline", "omp", "--", "--env", "dev"]);
        assert_eq!(request.env_name, None);
        assert_eq!(request.args, argv(&["--env", "dev"]));

        let request = harness_run(&["rayline", "hermes", "--help"]);
        assert_eq!(request.args, argv(&["--help"]));
        assert!(rayline_help_for_argv(&argv(&["rayline", "hermes", "--help"])).is_none());

        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "openclaw", "--env", "bad/env"])),
            RaylineDispatch::Usage(_)
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "openclaw", "--env"])),
            RaylineDispatch::Usage(_)
        ));
    }

    #[test]
    fn codex_reads_env_before_its_own_arguments() {
        match rayline_dispatch_for_argv(&argv(&["raydock", "codex", "--env", "dev", "--", "exec"]))
        {
            RaylineDispatch::CodexRun(request) => {
                assert_eq!(request.env_name.as_deref(), Some("dev"));
                assert!(request.root_env_explicit);
                assert_eq!(request.codex_args, argv(&["exec"]));
            }
            other => panic!("expected CodexRun, got {other:?}"),
        }
        match rayline_dispatch_for_argv(&argv(&["raydock", "codex", "app", "--env=dev"])) {
            RaylineDispatch::CodexApp(request) => {
                assert_eq!(request.env_name.as_deref(), Some("dev"));
                assert!(request.root_env_explicit);
            }
            other => panic!("expected CodexApp, got {other:?}"),
        }
        // After the first argument Raydock does not own, `--env` is Codex's.
        match rayline_dispatch_for_argv(&argv(&["raydock", "codex", "exec", "--env", "dev"])) {
            RaylineDispatch::CodexRun(request) => {
                assert_eq!(request.env_name, None);
                assert_eq!(request.codex_args, argv(&["exec", "--env", "dev"]));
            }
            other => panic!("expected CodexRun, got {other:?}"),
        }
    }
}
