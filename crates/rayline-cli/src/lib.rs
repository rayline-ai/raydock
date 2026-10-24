use std::env;
use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

pub(crate) mod auth_callback_page;
pub mod catalog;
pub mod claude;
pub mod claude_app;
pub(crate) mod claude_daemon;
pub mod codex;
pub mod codex_app;
pub mod discover;
pub mod harness;
pub mod local_model;
pub mod onboarding;
pub mod providers;
pub mod router;
pub mod router_config;
pub mod status;
pub mod update;

pub const CLI_BIN: &str = "rayline";
pub const DAEMON_BIN: &str = "rld";
pub const DISPLAY_NAME: &str = "Rayline";
pub const CONFIG_DIR: &str = "rayline";
pub const DOT_CONFIG_DIR: &str = ".rayline";
pub const ROUTER_STATE_DIR: &str = ".rayline/rld";
pub const ROUTER_FILE_PREFIX: &str = "rl-rld";
pub const ROUTER_PROD_URL: &str = "https://api.rayline.ai";
pub const UPDATE_BASE_URL: &str = "https://get.rayline.ai";
pub const INSTALLER_URL: &str = "https://get.rayline.ai/install.sh";
pub const UV_TOOL_NAME: &str = "rayline-cli";
pub const CLAUDE_LAUNCHES_SUFFIX: &str = ".config/rayline/claude-daemon-launches.json";
pub const RAYLINE_VERSION: &str = env!("RAYLINE_VERSION");
pub const RAYLINE_CHANNEL: &str = env!("RAYLINE_CHANNEL");

// Production minisign public key(s) for release / self-update signature verification.
// The matching secret key lives only in the GitHub `release` environment secret
// MINISIGN_SECRET_KEY (see RELEASING-SIGNING.md). Rotation: add the next public key
// alongside the current one (verification accepts any listed key), ship a release, then
// retire the old key after users have updated.
pub const MINISIGN_PUBLIC_KEYS: &[&str] = &[
    "RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1", // rayline production signing key (2026-06)
];

const ROOT_HELP: &str = "\
Usage: rayline [OPTIONS] COMMAND [ARGS]...

Options:
  --version    Show version
  --help       Show this message and exit

Commands:
  auth       Sign in to hosted Rayline
  status     Show current CLI auth status
  claude     Run Claude Code through Rayline routing
  claude-desktop  Run Claude Desktop's Code sessions on ARC (local router)
  codex      Run Codex CLI through Rayline local Responses routing
  opencode   Run opencode against the Rayline router
  pi         Run pi against the Rayline router
  omp        Run oh-my-pi (omp) against the Rayline router
  hermes     Run Hermes Agent against the Rayline router
  openclaw   Run OpenClaw against the Rayline router
  router     Start, inspect, or stop the local Rayline router runtime
  top        Show live router request metrics
  local      Configure local model routing
  update     Check for or install a rayline launcher update
";

const STATUS_HELP: &str = "\
Usage: rayline status

Show current CLI auth status.
";

const AUTH_HELP: &str = "\
Usage: rayline auth COMMAND

Commands:
  login    Sign in to Rayline
  logout   Revoke the stored Rayline session and router key
  status   Show current auth status
  token    Print a valid account bearer token
";

const AUTH_LOGIN_HELP: &str = "\
Usage: rayline auth login [OPTIONS]

Options:
  -b, --no-browser     Use device-code login instead of a local callback
  -p, --paste          Paste a browser callback URL manually
  --help               Show this message and exit
";

const AUTH_LOGOUT_HELP: &str = "\
Usage: rayline auth logout

Revoke the stored Rayline session and remove the stored router key.
";

const AUTH_STATUS_HELP: &str = "\
Usage: rayline auth status

Show current auth status.
";

const AUTH_TOKEN_HELP: &str = "\
Usage: rayline auth token

Print a valid account bearer token for scripts and integrations.
";

const CLAUDE_HELP: &str = "\
Usage: rayline claude [OPTIONS] [--] [CLAUDE_ARGS]...
       rayline claude run [OPTIONS] [--] [CLAUDE_ARGS]...

Run Claude Code through Rayline hosted routing. Use --local for the
local static router path.

Options:
  -m, --model <model>               Route through a specific model
  --auto-compact-window <tokens>    Override Claude auto-compact threshold
  --local                           Use local static routing without hosted auth
                                    (no login; forces the proxy)
  --local-provider <provider>       Use a local provider for subagents
                                    (ollama, lmstudio, llamacpp)
  --isolated                        Use a separate Claude config dir so this
                                    session can run beside other Claude Code
                                    background agents
  --route <all|subagents>           What the proxy routes through the router
                                    (default: all for cloud, subagents for local)
  --via <proxy|env>                 How Claude Code connects to the router
                                    (default: proxy; env is lightweight,
                                    cloud-only, no background process)
  --local-injector-port <port>      Local injector port
  --statusline/--no-statusline      Show proxy picked model in status line
  --diagnose                        Print routing diagnostics before exec
  --upstream-ca-path <path>         CA bundle for upstream proxy mode
  --config <path>                   Routing config (endpoints + routes) driving
                                    BOTH the main agent and subagents. Scope is
                                    derived from routes.main (a `subscription`
                                    main passes through; any endpoint routes it).
  --router-config-path <path>       Local-router static router JSON config
                                    (refines `--local`'s subagent allowlist)
  --help                            Show this message and exit
";

const CLAUDE_APP_HELP: &str = "\
Usage: rayline claude-desktop [COMMAND] [OPTIONS]

Run Claude Code inside the Claude desktop app on ARC. The import adds two
models to Desktop's picker: arc-sage (default) and arc-swift (cheaper). ARC is
not a Claude model, and Rayline is not affiliated with Anthropic. Starts a dedicated
local router (its own ports and state in ~/.rayline/claude-desktop, separate
from `rayline claude` and `rayline codex`), selects a recoverable Rayline ARC
profile, and opens Claude Desktop. A running Desktop is gracefully restarted
when its configuration changes. --manual prints import instructions instead.
Your Rayline key stays in the router; Desktop gets a gateway
token that only unlocks this local router. macOS only (the router also runs on
Linux, for testing).

Third-party mode applies to the whole Desktop app and switches away from your
claude.ai subscription; `rayline claude-desktop off` restores the previous mode.

Commands:
  start              Configure and launch Desktop with ARC (default)
  status             Show router health, request totals and the Desktop check
  validate           Check Desktop's applied configuration against the router
  stop               Stop the router (and unload the login service)
  install-service    Start the router at login (macOS LaunchAgent)
  uninstall-service  Remove the login service
  off                Restore the previous Desktop profile and mode; stop
  uninstall          Remove the service, the router state and its files

Options:
  --manual                  start: router only; print manual import instructions
  --prefixed-ids            start/install-service: forward the prefixed
                            rayline/arc-* aliases instead of the bare
                            arc-sage/arc-swift ids (for a router that does
                            not serve the bare ids yet)
  --desktop-config <path>   validate: check this file instead of Desktop's
  --help                    Show this message and exit
";

const CODEX_HELP: &str = "\
Usage: rayline codex [OPTIONS] [--] [CODEX_ARGS]...
       rayline codex app [OPTIONS] [WORKSPACE]
       rayline codex configure [OPTIONS]

Start Rayline's local OpenAI Responses router and run Codex CLI against it.

Options:
  --model <model>         Real model to request (e.g. gpt-5.6-luna). Omit to let the
                          router config decide (main/subagent routing).
  --config <path>         Rayline router config (endpoints + routes)
  --auth <mode>           Codex auth source: auto|subscription|none.
                          auto with no --config defaults to Rc-Rc (route
                          everything to the hosted cloud router, like
                          `rayline claude`); with --config it follows the
                          config. subscription uses your ChatGPT subscription;
                          none runs local.
  --subscription          Alias for --auth subscription
  --help                  Show this message and exit
";

const CODEX_APP_HELP: &str = "\
Usage: rayline codex app [OPTIONS] [WORKSPACE]

Start Rayline's local router and launch the Codex desktop app against it.

Unlike the CLI, the desktop app-server ignores -c overrides and is
single-instance, so this writes the Rayline provider into an isolated
CODEX_HOME (~/.rayline/codex-app-home) and launches the app there. If the app
is already running on a different config, you'll be prompted to restart it.

Options:
  --model <model>         Real model to request (e.g. gpt-5.6-luna). Omit to let the
                          router config decide (main/subagent routing).
  --config <path>         Rayline router config (endpoints + routes)
  --auth <mode>           Codex auth source: auto|subscription|none.
                          auto with no --config defaults to Rc-Rc (route
                          everything to the hosted cloud router). subscription
                          uses your ChatGPT subscription; none runs local.
  --subscription          Alias for --auth subscription
  --help                  Show this message and exit
";

const CODEX_CONFIGURE_HELP: &str = "\
Usage: rayline codex configure [OPTIONS]

Write a Codex profile at $CODEX_HOME/rayline.config.toml (or ~/.codex).

Options:
  --model <model>         Real model to request (e.g. gpt-5.6-luna). Omit to let the
                          router config decide (main/subagent routing).
  --base-url <url>        Rayline OpenAI Responses base URL
                          (default: http://127.0.0.1:20811/v1)
  --auth <mode>           Codex auth source: subscription|none
                          (default: subscription)
  --subscription          Alias for --auth subscription
  --help                  Show this message and exit
";

const ROUTER_HELP: &str = "\
Usage: rayline router COMMAND

Commands:
  start    Start router processes
  status   Show router status
  logs     Print router logs
  top      Show live router request metrics
  stop     Stop router processes
";

const ROUTER_START_HELP: &str = "\
Usage: rayline router start [--mode <anthropic|codex>] [--route <all|subagents>] [--config <path>]

Start the local router + transparent proxy daemon and exit, leaving it running.
Point an Anthropic SDK client at the proxy on http://127.0.0.1:20810 (and trust
the proxy CA cert) to route requests through it. The on-device model comes from
your `rayline local` configuration.

Options:
  --mode <anthropic|codex>  Inbound API surface (default: anthropic). `codex`
                            exposes OpenAI Responses at http://127.0.0.1:20811/v1.
  --auth <mode>             Codex auth source when --mode codex:
                            auto|subscription|none. auto uses subscription
                            when --config is absent and no client auth when
                            --config is present.
  --subscription            Alias for --auth subscription
  --route <all|subagents>   What the proxy routes through the router
                            (default: all). With `all`, request model
                            `rayline-local` to reach the on-device model.
  --config <path>           Routing config (endpoints + routes) driving BOTH
                            main and subagents; scope derived from routes.main.
                            Overrides --route.
  --help                    Show this message and exit
";

const ROUTER_STATUS_HELP: &str = "\
Usage: rayline router status

Show local router process status.
";

const ROUTER_LOGS_HELP: &str = "\
Usage: rayline router logs [--lines <count>]

Print local router logs.
";

const ROUTER_TOP_HELP: &str = "\
Usage: rayline top [--json] [--all]
       rayline router top [--json] [--all]

Show live LLM request metrics.

Options:
  --json      Print one snapshot as JSON
  --all       Include proxied Anthropic sideband traffic
";

const ROUTER_STOP_HELP: &str = "\
Usage: rayline router stop

Stop local router processes.
";

const LOCAL_HELP: &str = "\
Usage: rayline local COMMAND

Configure local model routing for this machine.

Commands:
  models    List provider and recommended models with hardware fit
  download  Download a recommended model without selecting it
  use       Select a recommended model, downloading it first if needed
  remove    Delete a downloaded model from disk
  custom    Use a custom endpoint URL + model name
  show      Show the configured mode, endpoint, and account routing state
  test      Probe a custom/provider endpoint for a compatible API response
  clear     Remove the local model configuration
  on        Turn local routing on for your account
  off       Turn local routing off for your account
  onboard   Set up or re-pick a local model for `rayline claude --local`
  subagents Discover your subagents and map each to local or cloud
";

const LOCAL_MODELS_HELP: &str = "\
Usage: rayline local models [--json]

List provider and recommended local models.
";

const LOCAL_DOWNLOAD_HELP: &str = "\
Usage: rayline local download <model-id> [--json]

Download a recommended model into the local cache without selecting it.
";

const LOCAL_USE_HELP: &str = "\
Usage: rayline local use <number|model-id>

Select a provider model or recommended built-in model.
Numbers come from `rayline local models`.
";

const LOCAL_REMOVE_HELP: &str = "\
Usage: rayline local remove <model-id>

Delete a downloaded model's file from the local cache.
";

const LOCAL_CUSTOM_HELP: &str = "\
Usage: rayline local custom [--url <URL>] [--model <NAME>]

Use a custom local inference endpoint.
";

const LOCAL_SHOW_HELP: &str = "\
Usage: rayline local show

Show the configured local model mode and account routing state.
";

const LOCAL_TEST_HELP: &str = "\
Usage: rayline local test [--url <URL>] [--model <NAME>]

Probe a custom endpoint for protocol compatibility.
";

const LOCAL_CLEAR_HELP: &str = "\
Usage: rayline local clear

Remove the local model configuration.
";

const LOCAL_ON_HELP: &str = "\
Usage: rayline local on

Turn local routing on for your account.
";

const LOCAL_OFF_HELP: &str = "\
Usage: rayline local off

Turn local routing off for your account.
";

const LOCAL_ONBOARD_HELP: &str = "\
Usage: rayline local onboard [--reset]

Set up (or re-pick) a local model for `rayline claude --local`.

Options:
  --reset    Clear the current local model first, then run the wizard
";

const LOCAL_SUBAGENTS_HELP: &str = "\
Usage: rayline local subagents [--json] [--all]

Scan your Claude Code history for the subagents you use and map each to a
routing target (local or cloud) for `rayline claude --local`. Read-only agents
are pre-selected for local. The mapping is saved to settings.json and applied
on the next `--local` launch.

The interactive picker is single-keypress (no Enter needed): 16 rows per page
labeled 0-9a-f cycle that row's target; n/p page; k cycles the kind filter
(all / read-only / has-edit / unknown); / searches by agent name; L/C send the
whole filtered set to local/cloud; s (or Enter) saves; q (or Esc) quits.

Settings stay compact: only agents that differ from the default (cloud) are
saved, so a long tail of rarely-used agents costs nothing.

Options:
  --json    Print discovered agents and their default target as JSON; no prompt
  --all     Include rarely-used agents (otherwise the long tail is hidden)
";

const UPDATE_HELP: &str = "\
Usage: rayline update [OPTIONS]

Options:
  --check              Check for an update without installing
  --version <version>  Install or check a specific version
  --channel <channel>  Use prod, dev, main, or local
  --force              Install even when already current
  --dry-run            Download and verify without replacing rayline
";

pub async fn run() -> ExitCode {
    let original_argv = env::args_os().collect::<Vec<_>>();
    run_argv(&original_argv).await
}

pub async fn run_argv(original_argv: &[OsString]) -> ExitCode {
    if root_version_requested(original_argv) {
        println!("rayline {RAYLINE_VERSION}");
        return ExitCode::SUCCESS;
    }

    if let Some(help) = rayline_help_for_argv(original_argv) {
        print!("{help}");
        return ExitCode::SUCCESS;
    }

    match rayline_dispatch_for_argv(original_argv) {
        RaylineDispatch::Version => {
            println!("rayline {RAYLINE_VERSION}");
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
                    eprintln!("Error: Not logged in to {env_name}. Run: rayline auth login");
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
        RaylineDispatch::CodexConfigure(request) => match codex::configure(&request) {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: failed to configure Codex profile: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::RouterStart(request) => {
            match crate::router::start_from_cli(&request).await {
                Ok(message) => {
                    print!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::RouterStatus(request) => {
            match crate::router::render_status(&request).await {
                Ok(message) => {
                    print!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::RouterLogs(request) => match crate::router::render_logs(&request) {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::RouterTop(request) => match crate::router::render_top(&request).await {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::RouterStop(request) => match crate::router::stop(&request).await {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalModels { env_name, json } => {
            let color =
                !json && std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            match catalog::models_command(env_name.as_deref(), json, color).await {
                Ok(message) => {
                    print!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => local_error(error, json),
            }
        }
        RaylineDispatch::LocalDownload {
            env_name,
            model_id,
            json,
        } => match catalog::download_command(env_name.as_deref(), &model_id, json).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => local_error(error, json),
        },
        RaylineDispatch::LocalUse { env_name, model_id } => {
            match catalog::use_command(env_name.as_deref(), &model_id).await {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalRemove { env_name, model_id } => {
            match catalog::remove_command(env_name.as_deref(), &model_id).await {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalCustom(request) => match local_model::set_custom(&request) {
            Ok(config) => {
                let url = config.base_url.as_deref().unwrap_or("(not set)");
                let model = config.model.as_deref().unwrap_or("(not set)");
                println!("Local model set to custom endpoint.\n  URL:    {url}\n  Model:  {model}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalShow {
            env_name,
            auth_token,
        } => {
            print!(
                "{}",
                local_model::render_show(env_name.as_deref(), auth_token.as_deref()).await
            );
            ExitCode::SUCCESS
        }
        RaylineDispatch::LocalTest(request) => match local_model::test(&request).await {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalClear => match local_model::clear() {
            Ok(true) => {
                println!("Local model configuration cleared.");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("No local model configuration was set.");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalOn {
            env_name,
            auth_token,
        } => {
            match local_model::set_router_enabled(true, env_name.as_deref(), auth_token.as_deref())
                .await
            {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalOff {
            env_name,
            auth_token,
        } => {
            match local_model::set_router_enabled(false, env_name.as_deref(), auth_token.as_deref())
                .await
            {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalOnboard { env_name, reset } => {
            match onboarding::run_onboard_command(env_name.as_deref(), reset).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalSubagents { json, all } => {
            match discover::run_subagents_command(json, all).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
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
        RaylineDispatch::Unavailable => unavailable(original_argv),
    }
}

fn local_error(error: String, json: bool) -> ExitCode {
    if json {
        catalog::emit_error_json(&error);
    } else {
        eprintln!("Error: {error}");
    }
    ExitCode::from(1)
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
    RouterStart(router::RouterStartCliRequest),
    RouterStatus(router::RouterStatusRequest),
    RouterLogs(router::RouterLogsRequest),
    RouterTop(router::RouterTopRequest),
    RouterStop(router::RouterStopRequest),
    LocalModels {
        env_name: Option<String>,
        json: bool,
    },
    LocalDownload {
        env_name: Option<String>,
        model_id: String,
        json: bool,
    },
    LocalUse {
        env_name: Option<String>,
        model_id: String,
    },
    LocalRemove {
        env_name: Option<String>,
        model_id: String,
    },
    LocalCustom(local_model::LocalCustomRequest),
    LocalShow {
        env_name: Option<String>,
        auth_token: Option<String>,
    },
    LocalTest(local_model::LocalTestRequest),
    LocalClear,
    LocalOn {
        env_name: Option<String>,
        auth_token: Option<String>,
    },
    LocalOff {
        env_name: Option<String>,
        auth_token: Option<String>,
    },
    LocalOnboard {
        env_name: Option<String>,
        reset: bool,
    },
    LocalSubagents {
        json: bool,
        all: bool,
    },
    Update(update::UpdateRequest),
    Unavailable,
}

pub fn rayline_dispatch_for_argv(original_argv: &[OsString]) -> RaylineDispatch {
    if root_version_requested(original_argv) {
        return RaylineDispatch::Version;
    }

    let mut root_env = None;
    let mut root_auth_token = None;
    let mut root_env_explicit = false;
    let mut args = original_argv.iter().skip(1).peekable();

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return RaylineDispatch::Unavailable;
        };

        if arg == "--" {
            return RaylineDispatch::Unavailable;
        }
        if let Some((name, value)) = arg.split_once('=') {
            match name {
                "--env" => {
                    if !status::is_valid_root_env(value) {
                        return RaylineDispatch::Unavailable;
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
            let Some(value) = args.next() else {
                return RaylineDispatch::Unavailable;
            };
            if arg == "--env" {
                let Some(value) = value.to_str() else {
                    return RaylineDispatch::Unavailable;
                };
                if !status::is_valid_root_env(value) {
                    return RaylineDispatch::Unavailable;
                }
                root_env = Some(value.to_owned());
                root_env_explicit = true;
            } else if arg == "--auth-token" {
                let Some(value) = value.to_str() else {
                    return RaylineDispatch::Unavailable;
                };
                root_auth_token = Some(value.to_owned());
            }
            continue;
        }
        if arg == "--version" || arg == "--help" {
            continue;
        }
        if arg.starts_with('-') {
            return RaylineDispatch::Unavailable;
        }

        return match arg {
            "auth" => parse_auth_dispatch(args, root_env, root_auth_token, root_env_explicit)
                .unwrap_or(RaylineDispatch::Unavailable),
            "claude-desktop" => {
                parse_claude_app_request(args, root_env, root_auth_token, root_env_explicit)
                    .map(RaylineDispatch::ClaudeApp)
                    .unwrap_or(RaylineDispatch::Unavailable)
            }
            "claude" => parse_claude_dispatch(args, root_env, root_auth_token, root_env_explicit)
                .unwrap_or(RaylineDispatch::Unavailable),
            "codex" => parse_codex_dispatch(args, root_env, root_auth_token, root_env_explicit)
                .unwrap_or(RaylineDispatch::Unavailable),
            "local" => parse_local_dispatch(args, root_env, root_auth_token)
                .unwrap_or(RaylineDispatch::Unavailable),
            "router" => parse_router_dispatch(args, root_env_explicit)
                .unwrap_or(RaylineDispatch::Unavailable),
            "status" => parse_status_request(args, root_env, root_auth_token, root_env_explicit)
                .map(RaylineDispatch::Status)
                .unwrap_or(RaylineDispatch::Unavailable),
            "top" => parse_router_top_request(args, root_env_explicit)
                .map(RaylineDispatch::RouterTop)
                .unwrap_or(RaylineDispatch::Unavailable),
            "update" => parse_update_request(args)
                .map(RaylineDispatch::Update)
                .unwrap_or(RaylineDispatch::Unavailable),
            command => match harness::Harness::from_command(command) {
                Some(harness) => parse_harness_request(
                    harness,
                    args,
                    root_env,
                    root_auth_token,
                    root_env_explicit,
                )
                .map(RaylineDispatch::Harness)
                .unwrap_or(RaylineDispatch::Unavailable),
                None => RaylineDispatch::Unavailable,
            },
        };
    }

    RaylineDispatch::Unavailable
}

/// `rayline [--env E] <harness> [--env E] [--auth-token T] [--] [ARGS]...`.
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
    let mut model = None;
    let mut auto_compact_window = None;
    let mut local_provider = None;
    let mut local_router = false;
    let mut isolated = false;
    let mut local_injector_port = None;
    // Two-axis routing intent, resolved into a RoutingMode at the end. `via`
    // selects the connection mechanism (proxy is the default; env is opt-in);
    // `route_scope` selects what the proxy routes (all vs subagents-only).
    let mut via: Option<ViaArg> = None;
    let mut route_scope: Option<RouteScope> = None;
    let mut route_statusline_enabled = true;
    let mut diagnose = false;
    let mut upstream_ca_path = None;
    let mut router_config_path = None;
    // v2: `--config <file>` drives BOTH main + subagents via a RouterConfig file,
    // engaging the on-device router without `--local`. Distinct from the legacy
    // `--router-config-path` (which only refines `--local`'s subagent allowlist).
    let mut config_path = None;
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
        if let Some((option, value)) = arg_str.split_once('=') {
            match option {
                "--env" => {
                    env_name = Some(value.to_owned());
                    continue;
                }
                "--model" => {
                    model = Some(value.to_owned());
                    continue;
                }
                "--auto-compact-window" => {
                    auto_compact_window = Some(value.parse().ok()?);
                    continue;
                }
                "--local-provider" => {
                    local_provider = Some(crate::providers::ProviderId::parse(value)?);
                    local_router = true;
                    continue;
                }
                "--via" => {
                    via = Some(parse_via(value)?);
                    continue;
                }
                "--route" => {
                    route_scope = Some(parse_route_scope(value)?);
                    continue;
                }
                "--routing-mode" => {
                    apply_deprecated_routing_mode(value, &mut via, &mut route_scope)?;
                    continue;
                }
                "--local-injector-port" => {
                    local_injector_port = Some(value.parse().ok()?);
                    continue;
                }
                "--upstream-ca-path" => {
                    upstream_ca_path = Some(PathBuf::from(value));
                    continue;
                }
                "--router-config-path" => {
                    router_config_path = Some(PathBuf::from(value));
                    continue;
                }
                "--config" => {
                    config_path = Some(PathBuf::from(value));
                    continue;
                }
                _ => {}
            }
        }
        match arg_str {
            "--env" => {
                env_name = Some(args.next()?.to_str()?.to_owned());
                continue;
            }
            "--model" | "-m" => {
                model = Some(args.next()?.to_str()?.to_owned());
                continue;
            }
            "--auto-compact-window" => {
                auto_compact_window = Some(args.next()?.to_str()?.parse().ok()?);
                continue;
            }
            "--local-provider" => {
                local_provider = Some(crate::providers::ProviderId::parse(args.next()?.to_str()?)?);
                local_router = true;
                continue;
            }
            "--via" => {
                via = Some(parse_via(args.next()?.to_str()?)?);
                continue;
            }
            "--route" => {
                route_scope = Some(parse_route_scope(args.next()?.to_str()?)?);
                continue;
            }
            "--routing-mode" => {
                apply_deprecated_routing_mode(args.next()?.to_str()?, &mut via, &mut route_scope)?;
                continue;
            }
            "--local" | "--local-router" => {
                local_router = true;
                continue;
            }
            "--isolated" => {
                isolated = true;
                continue;
            }
            "--no-proxy" => {
                // Deprecated alias for `--via env`.
                eprintln!("Warning: `--no-proxy` is deprecated; use `{CLI_BIN} claude --via env`.");
                via = Some(ViaArg::Env);
                continue;
            }
            "--diagnose" => {
                diagnose = true;
                continue;
            }
            "--local-injector-port" => {
                local_injector_port = Some(args.next()?.to_str()?.parse().ok()?);
                continue;
            }
            "--upstream-ca-path" => {
                upstream_ca_path = Some(PathBuf::from(args.next()?));
                continue;
            }
            "--router-config-path" => {
                router_config_path = Some(PathBuf::from(args.next()?));
                continue;
            }
            "--config" => {
                config_path = Some(PathBuf::from(args.next()?));
                continue;
            }
            "--no-telemetry" => {
                return None;
            }
            "--statusline" => {
                route_statusline_enabled = true;
                continue;
            }
            "--no-statusline" => {
                route_statusline_enabled = false;
                continue;
            }
            _ => {}
        }

        if claude_args.is_empty() && is_claude_management_subcommand(arg_str) {
            return None;
        }
        claude_args.push(arg.clone());
    }

    let local_provider_model = if matches!(
        local_provider,
        Some(crate::providers::ProviderId::Ollama | crate::providers::ProviderId::LmStudio)
    ) {
        model.take()
    } else {
        None
    };
    // `--config` engages the on-device router; `--via env` is cloud-only by
    // contract (it sets ANTHROPIC_BASE_URL directly and cannot reach local
    // inference). Reject the combination, mirroring `--via env` + `--local`.
    if config_path.is_some() && matches!(via, Some(ViaArg::Env)) {
        return None;
    }
    let routing_mode = resolve_routing_mode(local_router, via, route_scope)?;

    Some(crate::claude::RunRequest {
        env_name,
        auth_token: root_auth_token,
        args: claude_args,
        model,
        local_provider,
        local_provider_model,
        auto_compact_window,
        local_router,
        isolated,
        local_injector_port,
        routing_mode,
        route_scope_explicit: route_scope.is_some(),
        route_statusline_enabled,
        diagnose,
        upstream_ca_path,
        router_config_path,
        config_path,
        root_env_explicit,
    })
}

/// Connection mechanism wiring Claude Code to the router (the `--via` axis).
/// `proxy` is the default; `env` is the lightweight, cloud-only opt-in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViaArg {
    Env,
    Proxy,
}

/// What the proxy routes (the `--route` axis).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteScope {
    All,
    Subagents,
}

fn parse_via(value: &str) -> Option<ViaArg> {
    match value {
        "env" => Some(ViaArg::Env),
        "proxy" => Some(ViaArg::Proxy),
        _ => None,
    }
}

fn parse_route_scope(value: &str) -> Option<RouteScope> {
    match value {
        "all" => Some(RouteScope::All),
        "subagents" => Some(RouteScope::Subagents),
        _ => None,
    }
}

/// Map the deprecated `--routing-mode <value>` onto the two new axes, emitting a
/// one-line deprecation warning that names the replacement.
fn apply_deprecated_routing_mode(
    value: &str,
    via: &mut Option<ViaArg>,
    route_scope: &mut Option<RouteScope>,
) -> Option<()> {
    match value {
        "override" => {
            eprintln!(
                "Warning: `--routing-mode override` is deprecated; use `{CLI_BIN} claude --via env`."
            );
            *via = Some(ViaArg::Env);
        }
        "proxy" => {
            eprintln!(
                "Warning: `--routing-mode proxy` is deprecated; use `{CLI_BIN} claude --route all`."
            );
            // Full-mode alias: restore the proxy mechanism so it overrides an
            // earlier `--no-proxy`/`--routing-mode override` (last-value-wins).
            *via = Some(ViaArg::Proxy);
            *route_scope = Some(RouteScope::All);
        }
        "proxy-subagents" => {
            eprintln!(
                "Warning: `--routing-mode proxy-subagents` is deprecated; use `{CLI_BIN} claude --route subagents`."
            );
            // Full-mode alias: restore the proxy mechanism so it overrides an
            // earlier `--no-proxy`/`--routing-mode override` (last-value-wins).
            *via = Some(ViaArg::Proxy);
            *route_scope = Some(RouteScope::Subagents);
        }
        _ => return None,
    }
    Some(())
}

/// Resolve the two-axis routing intent into the internal [`RoutingMode`].
///
/// The connection mechanism defaults to the proxy; `--via env` is the only way
/// to reach the env-override path, and it is rejected when the session needs
/// the proxy (local inference or selective routing). The proxy scope default is
/// router-dependent: cloud routes everything, local routes subagents only.
fn resolve_routing_mode(
    local_router: bool,
    via: Option<ViaArg>,
    route_scope: Option<RouteScope>,
) -> Option<crate::claude::RoutingMode> {
    use crate::claude::RoutingMode;

    let scope = route_scope.unwrap_or(if local_router {
        RouteScope::Subagents
    } else {
        RouteScope::All
    });

    match via {
        Some(ViaArg::Env) => {
            // The env mechanism is cloud-only and cannot route selectively.
            if local_router {
                eprintln!(
                    "Error: `--via env` cannot reach local inference; drop `--local` or `--via env`."
                );
                return None;
            }
            if matches!(route_scope, Some(RouteScope::Subagents)) {
                eprintln!(
                    "Error: `--via env` cannot route selectively; selective routing needs the proxy."
                );
                return None;
            }
            Some(RoutingMode::Override)
        }
        Some(ViaArg::Proxy) | None => Some(match scope {
            RouteScope::All => RoutingMode::Proxy,
            RouteScope::Subagents => RoutingMode::ProxySubagents,
        }),
    }
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
    args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<crate::claude_app::AppRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut action = None;
    let mut desktop_config = None;
    let mut manual = false;
    let mut id_style = crate::claude_app::IdStyle::Bare;
    let mut args = args;
    while let Some(arg) = args.next() {
        let arg_str = arg.to_str()?;
        if let Some((option, value)) = arg_str.split_once('=') {
            match option {
                "--desktop-config" => {
                    desktop_config = Some(PathBuf::from(value));
                    continue;
                }
                _ => return None,
            }
        }
        match arg_str {
            "--desktop-config" => desktop_config = Some(PathBuf::from(args.next()?)),
            "--prefixed-ids" => id_style = crate::claude_app::IdStyle::Prefixed,
            "--manual" => manual = true,
            _ if arg_str.starts_with('-') => return None,
            _ if action.is_none() => action = Some(crate::claude_app::AppAction::parse(arg_str)?),
            _ => return None,
        }
    }
    let action = action.unwrap_or(crate::claude_app::AppAction::Start);
    if manual && action != crate::claude_app::AppAction::Start {
        return None;
    }
    if desktop_config.is_some() && !matches!(action, crate::claude_app::AppAction::Validate) {
        return None;
    }
    if id_style == crate::claude_app::IdStyle::Prefixed
        && !matches!(
            action,
            crate::claude_app::AppAction::Start
                | crate::claude_app::AppAction::Serve
                | crate::claude_app::AppAction::InstallService
        )
    {
        return None;
    }
    Some(crate::claude_app::AppRequest {
        action,
        manual,
        desktop_config,
        id_style,
        env_name: root_env,
        auth_token: root_auth_token,
        root_env_explicit,
    })
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
            parse_codex_configure_request(args).map(RaylineDispatch::CodexConfigure)
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
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<crate::codex_app::AppRunRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model = None;
    let mut config_path = None;
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
                "--model" => {
                    model = Some(value.to_owned());
                    continue;
                }
                "--config" => {
                    config_path = Some(PathBuf::from(value));
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
            "--model" => model = Some(args.next()?.to_str()?.to_owned()),
            "--config" => config_path = Some(PathBuf::from(args.next()?)),
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
        config_path,
        auth_mode,
        codex_args,
        root_env_explicit,
        env_name: root_env,
        auth_token: root_auth_token,
    })
}

fn parse_codex_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<crate::codex::RunRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model = None;
    let mut config_path = None;
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
                "--model" => {
                    model = Some(value.to_owned());
                    continue;
                }
                "--config" => {
                    config_path = Some(PathBuf::from(value));
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
            "--model" => {
                model = Some(args.next()?.to_str()?.to_owned());
            }
            "--config" => {
                config_path = Some(PathBuf::from(args.next()?));
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
        config_path,
        auth_mode,
        codex_args,
        root_env_explicit,
        env_name: root_env,
        auth_token: root_auth_token,
    })
}

fn parse_codex_configure_request<'a, I>(
    mut args: std::iter::Peekable<I>,
) -> Option<crate::codex::ConfigureRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model = None;
    let mut base_url = None;
    let mut auth_mode = crate::codex::CodexAuthMode::Subscription;
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
    })
}

fn parse_router_dispatch<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env_explicit: bool,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    let first_arg = args.next()?;
    match first_arg.to_str()? {
        "start" => {
            parse_router_start_request(args, root_env_explicit).map(RaylineDispatch::RouterStart)
        }
        "status" => {
            if args.next().is_some() {
                return None;
            }
            Some(RaylineDispatch::RouterStatus(
                crate::router::RouterStatusRequest { root_env_explicit },
            ))
        }
        "logs" => {
            parse_router_logs_request(args, root_env_explicit).map(RaylineDispatch::RouterLogs)
        }
        "top" => parse_router_top_request(args, root_env_explicit).map(RaylineDispatch::RouterTop),
        "stop" => {
            if args.next().is_some() {
                return None;
            }
            Some(RaylineDispatch::RouterStop(
                crate::router::RouterStopRequest { root_env_explicit },
            ))
        }
        value if value.starts_with("--") => {
            let mut start_args = vec![first_arg];
            start_args.extend(args);
            parse_router_start_request(start_args.into_iter().peekable(), root_env_explicit)
                .map(RaylineDispatch::RouterStart)
        }
        _ => None,
    }
}

fn parse_router_start_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env_explicit: bool,
) -> Option<crate::router::RouterStartCliRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    // `router start` is always local; default to routing everything so a plain
    // SDK client can reach the on-device model via model-name routing.
    let mut route_scope = RouteScope::All;
    // v2: `--config <file>` drives BOTH main + subagents for headless/agent use;
    // scope is then derived from the config's `routes.main`, so `--route` is ignored.
    let mut config_path = None;
    let mut api_mode = crate::router::ROUTER_API_MODE_ANTHROPIC.to_owned();
    let mut codex_auth_mode = crate::codex::CodexAuthMode::Auto;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        // Accepted for symmetry with the routing surface; the router daemon is
        // inherently local, so this is a no-op.
        if arg == "--local" {
            continue;
        }
        if let Some(value) = arg.strip_prefix("--mode=") {
            api_mode = parse_router_api_mode(value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--auth=") {
            codex_auth_mode = crate::codex::CodexAuthMode::parse(value)?;
            continue;
        }
        if arg == "--mode" {
            api_mode = parse_router_api_mode(args.next()?.to_str()?)?;
            continue;
        }
        if arg == "--auth" {
            codex_auth_mode = crate::codex::CodexAuthMode::parse(args.next()?.to_str()?)?;
            continue;
        }
        if arg == "--subscription" {
            codex_auth_mode = crate::codex::CodexAuthMode::Subscription;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--route=") {
            route_scope = parse_route_scope(value)?;
            continue;
        }
        if arg == "--route" {
            route_scope = parse_route_scope(args.next()?.to_str()?)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            config_path = Some(PathBuf::from(value));
            continue;
        }
        if arg == "--config" {
            config_path = Some(PathBuf::from(args.next()?));
            continue;
        }
        return None;
    }
    let proxy_routing_mode = match route_scope {
        RouteScope::All => crate::router::PROXY_ROUTING_MODE_ALL,
        RouteScope::Subagents => crate::router::PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS,
    }
    .to_owned();
    Some(crate::router::RouterStartCliRequest {
        api_mode,
        proxy_routing_mode,
        config_path,
        codex_auth_mode,
        root_env_explicit,
        router_api_key_override: None,
    })
}

fn parse_router_api_mode(value: &str) -> Option<String> {
    match value {
        "anthropic" | "claude" => Some(crate::router::ROUTER_API_MODE_ANTHROPIC.to_owned()),
        "codex" | "openai" | "responses" | "openai_responses" => {
            Some(crate::router::ROUTER_API_MODE_CODEX.to_owned())
        }
        _ => None,
    }
}

fn parse_router_logs_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env_explicit: bool,
) -> Option<crate::router::RouterLogsRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut lines = 40;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;

        if arg == "--help" {
            return None;
        }
        if let Some(value) = arg.strip_prefix("-n") {
            if value.is_empty() {
                lines = args.next()?.to_str()?.parse().ok()?;
            } else {
                lines = value.parse().ok()?;
            }
            continue;
        }
        if let Some((option, value)) = arg.split_once('=') {
            if option == "--lines" {
                lines = value.parse().ok()?;
                continue;
            }
        }
        if arg == "--lines" {
            lines = args.next()?.to_str()?.parse().ok()?;
            continue;
        }

        return None;
    }

    Some(crate::router::RouterLogsRequest {
        lines,
        root_env_explicit,
    })
}

fn parse_router_top_request<'a, I>(
    args: std::iter::Peekable<I>,
    root_env_explicit: bool,
) -> Option<crate::router::RouterTopRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut json = false;
    let mut show_all = false;
    for arg in args {
        let arg = arg.to_str()?;
        match arg {
            "--json" => json = true,
            "--all" => show_all = true,
            "--help" => return None,
            _ => return None,
        }
    }

    Some(crate::router::RouterTopRequest {
        json,
        show_all,
        root_env_explicit,
    })
}

fn parse_update_request<'a, I>(mut args: std::iter::Peekable<I>) -> Option<update::UpdateRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut channel = None;
    let mut pinned_version = None;
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
                "--channel" => channel = Some(value.to_owned()),
                "--version" => pinned_version = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--channel" => channel = Some(args.next()?.to_str()?.to_owned()),
            "--version" => pinned_version = Some(args.next()?.to_str()?.to_owned()),
            "--force" => force = true,
            "--check" => check_only = true,
            "--dry-run" => dry_run = true,
            _ => return None,
        }
    }

    Some(update::UpdateRequest {
        channel,
        pinned_version,
        force,
        check_only,
        dry_run,
    })
}

fn parse_local_dispatch<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    match args.next()?.to_str()? {
        "models" => parse_local_json_flag(args).map(|json| RaylineDispatch::LocalModels {
            env_name: root_env,
            json,
        }),
        "download" => {
            parse_local_model_id_arg(args).map(|(model_id, json)| RaylineDispatch::LocalDownload {
                env_name: root_env,
                model_id,
                json,
            })
        }
        "use" => parse_local_model_id_arg(args).and_then(|(model_id, json)| {
            (!json).then_some(RaylineDispatch::LocalUse {
                env_name: root_env,
                model_id,
            })
        }),
        "remove" => parse_local_model_id_arg(args).and_then(|(model_id, json)| {
            (!json).then_some(RaylineDispatch::LocalRemove {
                env_name: root_env,
                model_id,
            })
        }),
        "custom" => parse_local_custom_request(args).map(RaylineDispatch::LocalCustom),
        "show" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalShow {
            env_name: root_env,
            auth_token: root_auth_token,
        }),
        "test" => parse_local_test_request(args).map(RaylineDispatch::LocalTest),
        "clear" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalClear),
        "on" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalOn {
            env_name: root_env,
            auth_token: root_auth_token,
        }),
        "off" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalOff {
            env_name: root_env,
            auth_token: root_auth_token,
        }),
        "onboard" => parse_local_onboard(args).map(|reset| RaylineDispatch::LocalOnboard {
            env_name: root_env,
            reset,
        }),
        "subagents" => parse_local_subagents(args)
            .map(|(json, all)| RaylineDispatch::LocalSubagents { json, all }),
        _ => None,
    }
}

fn parse_local_subagents<'a, I>(mut args: std::iter::Peekable<I>) -> Option<(bool, bool)>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut json = false;
    let mut all = false;
    for arg in args.by_ref() {
        match arg.to_str()? {
            "--json" => json = true,
            "--all" => all = true,
            "--help" => return None,
            _ => return None,
        }
    }
    Some((json, all))
}

fn parse_local_no_arg<'a, I>(mut args: std::iter::Peekable<I>) -> Option<()>
where
    I: Iterator<Item = &'a OsString>,
{
    args.next().is_none().then_some(())
}

fn parse_local_onboard<'a, I>(mut args: std::iter::Peekable<I>) -> Option<bool>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut reset = false;
    for arg in args.by_ref() {
        match arg.to_str()? {
            "--reset" => reset = true,
            "--help" => return None,
            _ => return None,
        }
    }
    Some(reset)
}

fn parse_local_json_flag<'a, I>(mut args: std::iter::Peekable<I>) -> Option<bool>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut json = false;
    for arg in args.by_ref() {
        match arg.to_str()? {
            "--json" => json = true,
            "--help" => return None,
            _ => return None,
        }
    }
    Some(json)
}

fn parse_local_model_id_arg<'a, I>(mut args: std::iter::Peekable<I>) -> Option<(String, bool)>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model_id = None;
    let mut json = false;
    for arg in args.by_ref() {
        match arg.to_str()? {
            "--json" => json = true,
            "--help" => return None,
            value if !value.starts_with('-') && model_id.is_none() => {
                model_id = Some(value.to_owned());
            }
            _ => return None,
        }
    }
    Some((model_id?, json))
}

fn parse_local_custom_request<'a, I>(
    mut args: std::iter::Peekable<I>,
) -> Option<local_model::LocalCustomRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut base_url = None;
    let mut model = None;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--url" | "--base-url" => base_url = Some(value.to_owned()),
                "--model" => model = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--url" | "--base-url" => base_url = Some(args.next()?.to_str()?.to_owned()),
            "--model" => model = Some(args.next()?.to_str()?.to_owned()),
            _ => return None,
        }
    }
    Some(local_model::LocalCustomRequest { base_url, model })
}

fn parse_local_test_request<'a, I>(
    mut args: std::iter::Peekable<I>,
) -> Option<local_model::LocalTestRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut base_url = None;
    let mut model = None;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--url" | "--base-url" => base_url = Some(value.to_owned()),
                "--model" => model = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--url" | "--base-url" => base_url = Some(args.next()?.to_str()?.to_owned()),
            "--model" => model = Some(args.next()?.to_str()?.to_owned()),
            _ => return None,
        }
    }
    Some(local_model::LocalTestRequest { base_url, model })
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
    eprintln!("rayline: failed to exec claude: {error}");
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rayline: failed to run claude: {error}");
            ExitCode::from(127)
        }
    }
}

fn unavailable(original_argv: &[OsString]) -> ExitCode {
    let command = first_command(original_argv).unwrap_or("command");
    eprintln!("rayline: `{command}` is not available in this public Rayline build");
    ExitCode::from(127)
}

fn rayline_help_for_argv(original_argv: &[OsString]) -> Option<&'static str> {
    let command = command_path_before_help(original_argv)?;
    match command.as_slice() {
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
        ["local"] => Some(LOCAL_HELP),
        ["local", "models"] => Some(LOCAL_MODELS_HELP),
        ["local", "download"] => Some(LOCAL_DOWNLOAD_HELP),
        ["local", "use"] => Some(LOCAL_USE_HELP),
        ["local", "remove"] => Some(LOCAL_REMOVE_HELP),
        ["local", "custom"] => Some(LOCAL_CUSTOM_HELP),
        ["local", "show"] => Some(LOCAL_SHOW_HELP),
        ["local", "test"] => Some(LOCAL_TEST_HELP),
        ["local", "clear"] => Some(LOCAL_CLEAR_HELP),
        ["local", "on"] => Some(LOCAL_ON_HELP),
        ["local", "off"] => Some(LOCAL_OFF_HELP),
        ["local", "onboard"] => Some(LOCAL_ONBOARD_HELP),
        ["local", "subagents"] => Some(LOCAL_SUBAGENTS_HELP),
        ["router"] => Some(ROUTER_HELP),
        ["router", "start"] => Some(ROUTER_START_HELP),
        ["router", "status"] => Some(ROUTER_STATUS_HELP),
        ["router", "logs"] => Some(ROUTER_LOGS_HELP),
        ["router", "top"] => Some(ROUTER_TOP_HELP),
        ["router", "stop"] => Some(ROUTER_STOP_HELP),
        ["status"] => Some(STATUS_HELP),
        ["top"] => Some(ROUTER_TOP_HELP),
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
        if let Some(rest) = arg.strip_prefix("-m") {
            if rest.is_empty() {
                args.next()?;
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        command.push(arg);
    }

    if command.is_empty() || matches!(command.as_slice(), ["auth"] | ["local"] | ["router"]) {
        Some(command)
    } else {
        None
    }
}

fn first_command(original_argv: &[OsString]) -> Option<&str> {
    let mut args = original_argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--" {
            return args.next().and_then(|arg| arg.to_str());
        }
        if is_value_option(arg) {
            if !arg.contains('=') {
                args.next()?;
            }
            continue;
        }
        if arg == "--version" || arg == "--help" || arg.starts_with('-') {
            continue;
        }
        return Some(arg);
    }
    None
}

fn is_value_option(arg: &str) -> bool {
    matches!(
        arg.split_once('=').map_or(arg, |(name, _)| name),
        "--env"
            | "--auth-token"
            | "--model"
            | "--local-provider"
            | "--auto-compact-window"
            | "--routing-mode"
            | "--via"
            | "--route"
            | "--local-injector-port"
            | "--upstream-ca-path"
            | "--router-config-path"
            | "--lines"
            | "--channel"
            | "--url"
            | "--base-url"
            | "--desktop-config"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    use crate::claude::RoutingMode;

    fn claude_run(args: &[&str]) -> crate::claude::RunRequest {
        let RaylineDispatch::ClaudeRun(request) = rayline_dispatch_for_argv(&argv(args)) else {
            panic!("expected ClaudeRun for {args:?}");
        };
        request
    }

    // ── Connection mechanism resolution (the two-axis model) ──────────────

    #[test]
    fn resolve_cloud_default_is_proxy_all() {
        assert_eq!(
            resolve_routing_mode(false, None, None),
            Some(RoutingMode::Proxy)
        );
    }

    #[test]
    fn resolve_cloud_route_subagents_is_proxy_subagents() {
        assert_eq!(
            resolve_routing_mode(false, None, Some(RouteScope::Subagents)),
            Some(RoutingMode::ProxySubagents)
        );
    }

    #[test]
    fn resolve_local_default_is_proxy_subagents() {
        assert_eq!(
            resolve_routing_mode(true, None, None),
            Some(RoutingMode::ProxySubagents)
        );
    }

    #[test]
    fn resolve_local_route_all_is_proxy_all() {
        assert_eq!(
            resolve_routing_mode(true, None, Some(RouteScope::All)),
            Some(RoutingMode::Proxy)
        );
    }

    #[test]
    fn resolve_via_env_cloud_is_override() {
        assert_eq!(
            resolve_routing_mode(false, Some(ViaArg::Env), None),
            Some(RoutingMode::Override)
        );
    }

    #[test]
    fn resolve_via_proxy_cloud_is_proxy_all() {
        assert_eq!(
            resolve_routing_mode(false, Some(ViaArg::Proxy), None),
            Some(RoutingMode::Proxy)
        );
    }

    #[test]
    fn resolve_via_env_with_local_is_rejected() {
        // Local inference is unreachable via the env mechanism.
        assert_eq!(resolve_routing_mode(true, Some(ViaArg::Env), None), None);
    }

    #[test]
    fn resolve_via_env_with_route_subagents_is_rejected() {
        // Selective routing needs the proxy.
        assert_eq!(
            resolve_routing_mode(false, Some(ViaArg::Env), Some(RouteScope::Subagents)),
            None
        );
    }

    // ── Parser surface ────────────────────────────────────────────────────

    #[test]
    fn bare_claude_defaults_to_proxy_all() {
        assert_eq!(
            claude_run(&["rayline", "claude"]).routing_mode,
            RoutingMode::Proxy
        );
    }

    #[test]
    fn via_env_flag_selects_override() {
        assert_eq!(
            claude_run(&["rayline", "claude", "--via", "env"]).routing_mode,
            RoutingMode::Override
        );
    }

    #[test]
    fn local_flag_forces_proxy_subagents() {
        let request = claude_run(&["rayline", "claude", "--local"]);
        assert!(request.local_router);
        assert_eq!(request.routing_mode, RoutingMode::ProxySubagents);
    }

    #[test]
    fn claude_accepts_local_provider() {
        let request = claude_run(&["rayline", "claude", "--local", "--local-provider", "ollama"]);
        assert!(request.local_router);
        assert_eq!(
            request.local_provider,
            Some(crate::providers::ProviderId::Ollama)
        );
    }

    #[test]
    fn claude_local_provider_implies_local() {
        let request = claude_run(&["rayline", "claude", "--local-provider", "lmstudio"]);
        assert!(request.local_router);
        assert_eq!(
            request.local_provider,
            Some(crate::providers::ProviderId::LmStudio)
        );
        assert_eq!(request.routing_mode, RoutingMode::ProxySubagents);
    }

    #[test]
    fn claude_local_provider_consumes_model_as_provider_model() {
        let request = claude_run(&[
            "rayline",
            "claude",
            "--local-provider",
            "ollama",
            "--model",
            "qwen3-coder:30b",
        ]);
        assert_eq!(
            request.local_provider,
            Some(crate::providers::ProviderId::Ollama)
        );
        assert_eq!(
            request.local_provider_model.as_deref(),
            Some("qwen3-coder:30b")
        );
        assert_eq!(request.model, None);
    }

    #[test]
    fn claude_local_provider_llamacpp_uses_builtin_path() {
        let request = claude_run(&[
            "rayline",
            "claude",
            "--local-provider",
            "llamacpp",
            "--model",
            "claude-sonnet-4-20250514",
        ]);
        assert!(request.local_router);
        assert_eq!(
            request.local_provider,
            Some(crate::providers::ProviderId::LlamaCpp)
        );
        assert_eq!(request.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(request.local_provider_model, None);
    }

    #[test]
    fn claude_rejects_unknown_local_provider() {
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "claude", "--local-provider", "bogus"])),
            RaylineDispatch::Unavailable
        ));
    }

    #[test]
    fn claude_rejects_local_provider_with_via_env() {
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&[
                "rayline",
                "claude",
                "--local-provider",
                "ollama",
                "--via",
                "env"
            ])),
            RaylineDispatch::Unavailable
        ));
    }

    #[test]
    fn route_subagents_flag_selects_proxy_subagents() {
        assert_eq!(
            claude_run(&["rayline", "claude", "--route", "subagents"]).routing_mode,
            RoutingMode::ProxySubagents
        );
    }

    #[test]
    fn deprecated_no_proxy_maps_to_override() {
        assert_eq!(
            claude_run(&["rayline", "claude", "--no-proxy"]).routing_mode,
            RoutingMode::Override
        );
    }

    #[test]
    fn deprecated_no_proxy_then_routing_mode_proxy_restores_proxy() {
        // Last-value-wins: a later deprecated full-mode alias must override an
        // earlier `--no-proxy`, restoring the proxy mechanism (not stay Override).
        assert_eq!(
            claude_run(&["rayline", "claude", "--no-proxy", "--routing-mode", "proxy"])
                .routing_mode,
            RoutingMode::Proxy
        );
        assert_eq!(
            claude_run(&[
                "rayline",
                "claude",
                "--no-proxy",
                "--routing-mode",
                "proxy-subagents"
            ])
            .routing_mode,
            RoutingMode::ProxySubagents
        );
    }

    #[test]
    fn via_env_with_local_is_unavailable() {
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "claude", "--via", "env", "--local"])),
            RaylineDispatch::Unavailable
        ));
    }

    #[test]
    fn claude_config_flag_parses_as_claude_run() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "claude",
            "--config",
            "/tmp/router.json",
        ])) {
            RaylineDispatch::ClaudeRun(request) => {
                assert_eq!(
                    request.config_path.as_deref(),
                    Some(std::path::Path::new("/tmp/router.json"))
                );
            }
            other => panic!("expected ClaudeRun, got {other:?}"),
        }
    }

    #[test]
    fn claude_config_flag_accepts_equals_form() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "claude", "--config=/tmp/r.json"])) {
            RaylineDispatch::ClaudeRun(request) => {
                assert_eq!(
                    request.config_path.as_deref(),
                    Some(std::path::Path::new("/tmp/r.json"))
                );
            }
            other => panic!("expected ClaudeRun, got {other:?}"),
        }
    }

    #[test]
    fn claude_via_env_with_config_is_unavailable() {
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&[
                "rayline",
                "claude",
                "--via",
                "env",
                "--config",
                "/tmp/router.json",
            ])),
            RaylineDispatch::Unavailable
        ));
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
            (&["rayline", "update", "--help"][..], "rayline update"),
            (
                &["rayline", "router", "status", "--help"][..],
                "rayline router status",
            ),
            (
                &["rayline", "router", "logs", "--help"][..],
                "rayline router logs",
            ),
            (
                &["rayline", "router", "top", "--help"][..],
                "rayline router top",
            ),
            (&["rayline", "top", "--help"][..], "rayline top"),
            (
                &["rayline", "router", "stop", "--help"][..],
                "rayline router stop",
            ),
            (&["rayline", "local", "--help"][..], "rayline local"),
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
    fn claude_help_documents_hosted_and_local_router_modes() {
        let help =
            rayline_help_for_argv(&argv(&["rayline", "claude", "--help"])).expect("claude help");

        assert!(help.contains("hosted routing"));
        assert!(help.contains("local static router"));
        assert!(help.contains("--local"));
        assert!(help.contains("--via"));
        assert!(help.contains("--route"));
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
        assert!(!request.local_router);
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
    fn public_parser_accepts_local_router_claude() {
        let dispatch = rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "claude",
            "--local-router",
            "--isolated",
            "-p",
            "hi",
        ]));

        let RaylineDispatch::ClaudeRun(request) = dispatch else {
            panic!("expected ClaudeRun");
        };
        assert!(request.local_router);
        assert!(request.isolated);
        assert_eq!(
            request
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["-p", "hi"]
        );
    }

    #[test]
    fn public_parser_accepts_auth_status_update_and_local() {
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
                channel: None,
                pinned_version: None,
                force: false,
                check_only: true,
                dry_run: true,
            })
        );
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "local", "models", "--json"])),
            RaylineDispatch::LocalModels {
                env_name: None,
                json: true,
            }
        );
    }

    #[test]
    fn public_parser_accepts_claude_run_alias() {
        let dispatch = rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "claude",
            "run",
            "--local-router",
            "--",
            "--debug",
        ]));

        let RaylineDispatch::ClaudeRun(request) = dispatch else {
            panic!("expected ClaudeRun");
        };
        assert!(request.local_router);
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
    fn public_parser_accepts_router_logs_lines() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "router", "logs", "--lines=7"])),
            RaylineDispatch::RouterLogs(crate::router::RouterLogsRequest {
                lines: 7,
                root_env_explicit: false,
            })
        );
    }

    #[test]
    fn public_parser_accepts_router_top_json() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&[
                "rayline", "--env", "foo", "router", "top", "--json"
            ])),
            RaylineDispatch::RouterTop(crate::router::RouterTopRequest {
                json: true,
                show_all: false,
                root_env_explicit: true,
            })
        );
    }

    #[test]
    fn public_parser_accepts_top_alias_json() {
        assert!(
            rayline_help_for_argv(&argv(&["rayline", "top"])).is_none(),
            "top alias should execute without requiring a subcommand"
        );
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&[
                "rayline", "--env", "foo", "top", "--json", "--all"
            ])),
            RaylineDispatch::RouterTop(crate::router::RouterTopRequest {
                json: true,
                show_all: true,
                root_env_explicit: true,
            })
        );
    }

    #[test]
    fn parser_rejects_invalid_hidden_env_names() {
        for env_name in ["", "foo.bar", "foo/bar", "foo bar"] {
            assert_eq!(
                rayline_dispatch_for_argv(&argv(&["rayline", "--env", env_name, "status"])),
                RaylineDispatch::Unavailable,
                "{env_name:?} should be rejected"
            );
        }
    }

    #[test]
    fn public_parser_accepts_local_onboard() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "local", "onboard"])),
            RaylineDispatch::LocalOnboard {
                env_name: None,
                reset: false
            }
        );
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "local", "onboard", "--reset"])),
            RaylineDispatch::LocalOnboard {
                env_name: None,
                reset: true
            }
        );
    }

    #[test]
    fn local_onboard_has_native_help() {
        assert!(rayline_help_for_argv(&argv(&["rayline", "local", "onboard", "--help"])).is_some());
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
            &["rayline", "claude", "--no-telemetry"][..],
            &["rayline", "claude", "setup"][..],
            &["rayline", "claude", "run", "setup"][..],
            &["rayline", "claude", "--model", "x", "hooks", "status"][..],
        ] {
            assert_eq!(
                rayline_dispatch_for_argv(&argv(args)),
                RaylineDispatch::Unavailable,
                "{args:?} should be unavailable"
            );
        }
    }

    #[test]
    fn router_start_dispatch_defaults_to_all_route() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "router", "start"])) {
            RaylineDispatch::RouterStart(request) => {
                assert_eq!(
                    request.proxy_routing_mode,
                    crate::router::PROXY_ROUTING_MODE_ALL
                );
                assert_eq!(request.api_mode, crate::router::ROUTER_API_MODE_ANTHROPIC);
                assert_eq!(request.codex_auth_mode, crate::codex::CodexAuthMode::Auto);
            }
            other => panic!("expected RouterStart, got {other:?}"),
        }
    }

    #[test]
    fn router_start_accepts_codex_mode_aliases() {
        for mode in ["codex", "openai", "responses"] {
            match rayline_dispatch_for_argv(&argv(&["rayline", "router", "start", "--mode", mode]))
            {
                RaylineDispatch::RouterStart(request) => {
                    assert_eq!(request.api_mode, crate::router::ROUTER_API_MODE_CODEX);
                }
                other => panic!("expected RouterStart for {mode}, got {other:?}"),
            }
        }
    }

    #[test]
    fn router_mode_shorthand_starts_router() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "router", "--mode", "openai"])) {
            RaylineDispatch::RouterStart(request) => {
                assert_eq!(request.api_mode, crate::router::ROUTER_API_MODE_CODEX);
            }
            other => panic!("expected RouterStart, got {other:?}"),
        }
    }

    #[test]
    fn router_start_accepts_codex_subscription_auth() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "router",
            "start",
            "--mode",
            "codex",
            "--auth",
            "subscription",
        ])) {
            RaylineDispatch::RouterStart(request) => {
                assert_eq!(request.api_mode, crate::router::ROUTER_API_MODE_CODEX);
                assert_eq!(
                    request.codex_auth_mode,
                    crate::codex::CodexAuthMode::Subscription
                );
            }
            other => panic!("expected RouterStart, got {other:?}"),
        }
    }

    #[test]
    fn router_start_dispatch_accepts_route_subagents() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "router",
            "start",
            "--route",
            "subagents",
        ])) {
            RaylineDispatch::RouterStart(request) => {
                assert_eq!(
                    request.proxy_routing_mode,
                    crate::router::PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS
                );
            }
            other => panic!("expected RouterStart, got {other:?}"),
        }
    }

    #[test]
    fn router_start_help_is_available() {
        assert!(rayline_help_for_argv(&argv(&["rayline", "router", "start", "--help"])).is_some());
    }

    #[test]
    fn router_start_rejects_unknown_route() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "router", "start", "--route", "bogus"])),
            RaylineDispatch::Unavailable,
        );
    }

    #[test]
    fn router_start_accepts_config_flag() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "router",
            "start",
            "--config",
            "/tmp/router.json",
        ])) {
            RaylineDispatch::RouterStart(request) => {
                assert_eq!(
                    request.config_path.as_deref(),
                    Some(std::path::Path::new("/tmp/router.json"))
                );
            }
            other => panic!("expected RouterStart, got {other:?}"),
        }
    }

    #[test]
    fn codex_dispatch_defaults_to_no_model_and_passes_args() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "codex", "--", "exec", "hello"])) {
            RaylineDispatch::CodexRun(request) => {
                // No `--model` → None, so the CLI stamps the virtual-marker sentinel
                // internally and the router applies config-driven routing.
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
        for args in [
            vec!["rayline", "claude-desktop"],
            vec!["rayline", "claude-desktop", "--manual"],
        ] {
            match rayline_dispatch_for_argv(&argv(&args)) {
                RaylineDispatch::ClaudeApp(request) => {
                    assert_eq!(request.action, crate::claude_app::AppAction::Start);
                    assert!(!request.root_env_explicit);
                    assert_eq!(request.manual, args.contains(&"--manual"));
                }
                other => panic!("expected ClaudeApp, got {other:?}"),
            }
        }
    }

    #[test]
    fn claude_app_dispatch_parses_subcommands_and_env() {
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "--env",
            "dev",
            "claude-desktop",
            "status",
        ])) {
            RaylineDispatch::ClaudeApp(request) => {
                assert_eq!(request.action, crate::claude_app::AppAction::Status);
                assert_eq!(request.env_name.as_deref(), Some("dev"));
                assert!(request.root_env_explicit);
            }
            other => panic!("expected ClaudeApp, got {other:?}"),
        }
        match rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "claude-desktop",
            "validate",
            "--desktop-config",
            "/tmp/d.json",
        ])) {
            RaylineDispatch::ClaudeApp(request) => {
                assert_eq!(request.action, crate::claude_app::AppAction::Validate);
                assert_eq!(
                    request.desktop_config.as_deref(),
                    Some(std::path::Path::new("/tmp/d.json"))
                );
            }
            other => panic!("expected ClaudeApp, got {other:?}"),
        }
    }

    #[test]
    fn claude_app_dispatch_parses_prefixed_ids_fallback() {
        match rayline_dispatch_for_argv(&argv(&["rayline", "claude-desktop", "--prefixed-ids"])) {
            RaylineDispatch::ClaudeApp(request) => {
                assert_eq!(request.action, crate::claude_app::AppAction::Start);
                assert_eq!(request.id_style, crate::claude_app::IdStyle::Prefixed);
            }
            other => panic!("expected ClaudeApp, got {other:?}"),
        }
        match rayline_dispatch_for_argv(&argv(&["rayline", "claude-desktop"])) {
            RaylineDispatch::ClaudeApp(request) => {
                assert_eq!(request.id_style, crate::claude_app::IdStyle::Bare);
            }
            other => panic!("expected ClaudeApp, got {other:?}"),
        }
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&[
                "rayline",
                "claude-desktop",
                "status",
                "--prefixed-ids"
            ])),
            RaylineDispatch::Unavailable
        );
    }

    #[test]
    fn claude_app_dispatch_rejects_unknown_input() {
        for args in [
            &["rayline", "claude-desktop", "bogus"][..],
            &["rayline", "claude-desktop", "off", "--manual"][..],
            &["rayline", "claude-desktop", "start", "stop"][..],
            &["rayline", "claude-desktop", "--local"][..],
            &["rayline", "claude-desktop", "--model", "rayline/arc-swift"][..],
            &[
                "rayline",
                "claude-desktop",
                "start",
                "--desktop-config",
                "/x",
            ][..],
        ] {
            assert_eq!(
                rayline_dispatch_for_argv(&argv(args)),
                RaylineDispatch::Unavailable,
                "{args:?}"
            );
        }
    }

    #[test]
    fn claude_without_app_still_runs_claude_code() {
        // `rayline claude app` stays Claude Code's: `app` is a prompt word.
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
            "--config",
            "/tmp/cfg.json",
            "--subscription",
            "/work/space",
        ])) {
            RaylineDispatch::CodexApp(request) => {
                assert_eq!(request.auth_mode, crate::codex::CodexAuthMode::Subscription);
                assert_eq!(
                    request.config_path.as_deref(),
                    Some(std::path::Path::new("/tmp/cfg.json"))
                );
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
                assert_eq!(request.auth_mode, crate::codex::CodexAuthMode::Subscription);
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

        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "openclaw", "--env", "bad/env"])),
            RaylineDispatch::Unavailable
        );
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "openclaw", "--env"])),
            RaylineDispatch::Unavailable
        );
    }
}
