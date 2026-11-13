//! The `rayline` command line (docs/spec.md sections 3, 4.1 and 9): parsing
//! and help text.
//!
//! [`parse`] turns the arguments into a [`Parsed`] or a [`UsageError`]. It
//! reads no files: the config is loaded once afterwards, from what it returns.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use crate::claude_app::AppAction;
use crate::codex::CodexAuthMode;
use crate::config::Protocol;
use crate::harness::Harness;
use crate::update::{UpdateRequest, Version};
use url::Url;

pub(crate) const ROOT_HELP: &str = "\
Usage: rayline [OPTIONS] COMMAND [ARGS]...

Options:
  --version        Show version
  --help           Show this message and exit

Commands:
  auth             Sign in, sign out, show status, print a token
  run              Launch a coding harness on ARC
  local            Start, stop or inspect the local Rayline process (supports hybrid mode)
  top              Show live request metrics
  update           Check for or install a Rayline update
";

pub(crate) const RUN_HELP: &str = "\
Usage: rayline run <HARNESS> [--local | --config <file>] [HARNESS ARGS]...

Harnesses:
  claude           Claude Code
  claude-desktop   Claude Desktop's Code sessions
  codex            Codex CLI
  opencode         opencode
  pi               pi
  omp              oh-my-pi
  hermes           Hermes Agent
  openclaw         OpenClaw
";

const AUTH_HELP: &str = "\
Usage: rayline auth COMMAND

Commands:
  login    Sign in to Rayline
  logout   Revoke the stored Rayline session and router key
  status   Show the signed-in account and environment
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

Show the signed-in account and environment.
";

const AUTH_TOKEN_HELP: &str = "\
Usage: rayline auth token

Print a valid account bearer token for scripts and integrations.
";

const CLAUDE_DESKTOP_HELP: &str = "\
Usage: rayline run claude-desktop [COMMAND] [--config <file>]

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

const CODEX_APP_HELP: &str = "\
Usage: rayline run codex app [OPTIONS] [WORKSPACE]

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
  --help                  Show this message and exit
";

const CODEX_CONFIGURE_HELP: &str = "\
Usage: rayline run codex configure [OPTIONS]

Write a Codex profile at $CODEX_HOME/rayline.config.toml (or ~/.codex) that
reaches ARC through the Rayline router with your router key.

Options:
  --model <model>         Model to request (default: your key's ARC Sage)
  --auth <mode>           Codex auth source: auto|subscription|none
                          (default: auto, your Rayline router key).
                          subscription and none are not available yet.
  --help                  Show this message and exit
";

const TOP_HELP: &str = "\
Usage: rayline top [--json]

Show live request metrics from the local process.

Options:
  --json    Print one snapshot as JSON
  --help    Show this message and exit
";

const UPDATE_HELP: &str = "\
Usage: rayline update [OPTIONS]

Options:
  --check              Check for an update without installing
  --version <version>  Install or check a specific version
  --force              Install even when already current
  --dry-run            Download and verify without replacing Rayline
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// Print this help and exit.
    Help(&'static str),
    /// `--version`.
    Version,
    /// The hidden `hybrid start|status|stop` the CI lifecycle scripts drive,
    /// with its arguments. Sub-PR 6b replaces it with `rayline local`.
    Hybrid(Vec<OsString>),
    Command {
        /// `--env <name>`. `None` is prod.
        env: Option<String>,
        command: Command,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Auth(Auth),
    Run(Run),
    Top {
        json: bool,
    },
    /// Boxed: a parsed version and URL make it much larger than the others.
    Update(Box<UpdateRequest>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Auth {
    Login { no_browser: bool, paste: bool },
    Logout,
    Status,
    Token,
}

/// `rayline run <HARNESS> [--local | --config <file>] ...`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub config: ConfigFlag,
    pub launch: Launch,
}

/// Which config a run reads (section 3.2).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfigFlag {
    /// Neither flag: the default file, cloud mode.
    Default,
    /// `--local`: the default file, local mode.
    Local,
    /// `--config <file>`: local mode if the file has `routing`.
    File(PathBuf),
}

/// The harness and its kept options (section 9).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Launch {
    Claude {
        isolated: bool,
        args: Vec<OsString>,
    },
    ClaudeDesktop(AppAction),
    Codex {
        options: CodexOptions,
        args: Vec<OsString>,
    },
    CodexApp {
        options: CodexOptions,
        args: Vec<OsString>,
    },
    CodexConfigure(CodexOptions),
    Harness {
        harness: Harness,
        args: Vec<OsString>,
    },
}

/// `--model` and `--auth` of `run codex`, `run codex app` and
/// `run codex configure`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CodexOptions {
    pub model: Option<String>,
    pub auth: Option<CodexAuthMode>,
}

impl Run {
    /// The option or command that local mode does not run yet, if this run
    /// uses one. Local mode runs what the deleted `hybrid run` ran: it
    /// connects to the running local process, which does not record its
    /// config file, so a `--config` file could not be checked against it
    /// (section 3.3 item 3). Sub-PR 6b lifts these refusals.
    pub(crate) fn not_in_local_mode(&self) -> Option<&'static str> {
        match self.config {
            ConfigFlag::File(_) => Some("`--config`"),
            ConfigFlag::Default | ConfigFlag::Local => self.launch.not_in_local_mode(),
        }
    }
}

impl Launch {
    /// The one protocol the harness speaks, the one its Rayline config sets
    /// up (spec section 5.2), checked against `endpoints` in a local-mode run.
    pub(crate) fn protocol(&self) -> Protocol {
        match self {
            Launch::Claude { .. } | Launch::ClaudeDesktop(_) => Protocol::AnthropicMessages,
            Launch::Codex { .. } | Launch::CodexApp { .. } | Launch::CodexConfigure(_) => {
                Protocol::OpenaiResponses
            }
            Launch::Harness { harness, .. } => match harness {
                Harness::OpenCode | Harness::Pi | Harness::Omp => Protocol::AnthropicMessages,
                Harness::Hermes => Protocol::OpenaiResponses,
                Harness::OpenClaw => Protocol::OpenaiChat,
            },
        }
    }

    /// The kept option or command that local mode does not run yet: the
    /// deleted `hybrid run` had none of them.
    fn not_in_local_mode(&self) -> Option<&'static str> {
        match self {
            Launch::Claude { isolated: true, .. } => Some("`--isolated`"),
            Launch::ClaudeDesktop(_) => Some("`run claude-desktop`"),
            Launch::CodexApp { .. } => Some("`run codex app`"),
            Launch::CodexConfigure(_) => Some("`run codex configure`"),
            Launch::Codex { options, .. } if options.model.is_some() => Some("`--model`"),
            Launch::Codex { options, .. } if options.auth.is_some() => Some("`--auth`"),
            Launch::Claude { .. } | Launch::Codex { .. } | Launch::Harness { .. } => None,
        }
    }
}

/// A rejected command line: the problem, the usage line of the command it
/// belongs to and where to read that command's help.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UsageError(String);

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A command with its own help: the words that name it and its help text.
#[derive(Clone, Copy)]
struct Help {
    words: &'static str,
    text: &'static str,
}

const ROOT: Help = Help {
    words: "",
    text: ROOT_HELP,
};
const RUN: Help = Help {
    words: " run",
    text: RUN_HELP,
};
const AUTH: Help = Help {
    words: " auth",
    text: AUTH_HELP,
};
const AUTH_LOGIN: Help = Help {
    words: " auth login",
    text: AUTH_LOGIN_HELP,
};
const AUTH_LOGOUT: Help = Help {
    words: " auth logout",
    text: AUTH_LOGOUT_HELP,
};
const AUTH_STATUS: Help = Help {
    words: " auth status",
    text: AUTH_STATUS_HELP,
};
const AUTH_TOKEN: Help = Help {
    words: " auth token",
    text: AUTH_TOKEN_HELP,
};
const CLAUDE_DESKTOP: Help = Help {
    words: " run claude-desktop",
    text: CLAUDE_DESKTOP_HELP,
};
const CODEX_APP: Help = Help {
    words: " run codex app",
    text: CODEX_APP_HELP,
};
const CODEX_CONFIGURE: Help = Help {
    words: " run codex configure",
    text: CODEX_CONFIGURE_HELP,
};
const TOP: Help = Help {
    words: " top",
    text: TOP_HELP,
};
const UPDATE: Help = Help {
    words: " update",
    text: UPDATE_HELP,
};

impl Help {
    fn error(self, problem: impl fmt::Display) -> UsageError {
        let usage = self
            .text
            .lines()
            .find(|line| line.starts_with("Usage:"))
            .unwrap_or_default();
        UsageError(format!(
            "rayline: {problem}\n\n{usage}\nRun `rayline{} --help` for details.",
            self.words
        ))
    }
}

/// The arguments after the program name.
struct Args<'a> {
    args: &'a [OsString],
}

impl<'a> Args<'a> {
    fn peek(&self) -> Option<&'a OsString> {
        self.args.first()
    }

    fn next(&mut self) -> Option<&'a OsString> {
        let (first, rest) = self.args.split_first()?;
        self.args = rest;
        Some(first)
    }

    fn rest(&mut self) -> Vec<OsString> {
        std::mem::take(&mut self.args).to_vec()
    }

    /// The next argument as text. Non-UTF-8 is rejected without echoing it.
    fn next_str(&mut self, help: Help) -> Result<Option<&'a str>, UsageError> {
        match self.next() {
            None => Ok(None),
            Some(arg) => arg
                .to_str()
                .map(Some)
                .ok_or_else(|| help.error("got an argument that is not valid UTF-8")),
        }
    }

    /// The value of option `name`: `inline` (`--name=value`) or the next
    /// argument. A missing, empty or option-like value is rejected.
    fn value(
        &mut self,
        name: &str,
        inline: Option<&'a str>,
        help: Help,
    ) -> Result<&'a str, UsageError> {
        let value = match inline {
            Some(value) => Some(value),
            None => match self.peek().and_then(|arg| arg.to_str()) {
                Some(value) if !value.starts_with('-') => {
                    self.next();
                    Some(value)
                }
                _ => None,
            },
        };
        value
            .filter(|value| !value.is_empty())
            .ok_or_else(|| help.error(format!("`{name}` needs a value")))
    }
}

/// `--name=value` split into the name and the value.
fn split_option(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((name, value)) if name.starts_with("--") => (name, Some(value)),
        _ => (arg, None),
    }
}

/// Whether `text` looks like a command or option word, and so may be echoed
/// in an error: a rejected argument may be a prompt or a key.
fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// `arg` quoted for an error if it may be echoed, else a placeholder.
fn shown(arg: &str) -> String {
    let (name, value) = split_option(arg);
    let bare = name.trim_start_matches('-');
    match value {
        _ if !is_word(bare) => "an argument".to_owned(),
        Some(_) => format!("`{name}=…`"),
        None => format!("`{name}`"),
    }
}

/// "unknown {kind} `arg`", naming `arg` only if it may be echoed.
fn unknown(kind: &str, arg: &str) -> String {
    match shown(arg).as_str() {
        "an argument" => format!("unknown {kind}"),
        shown => format!("unknown {kind} {shown}"),
    }
}

fn unexpected(help: Help, arg: &str) -> UsageError {
    if arg == "--env" || arg.starts_with("--env=") {
        return help.error(format!(
            "`--env` goes before the command: `rayline --env <name>{}`",
            help.words
        ));
    }
    help.error(format!("unexpected {}", shown(arg)))
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str, help: Help) -> Result<(), UsageError> {
    if slot.is_some() {
        return Err(help.error(format!("`{name}` given more than once")));
    }
    *slot = Some(value);
    Ok(())
}

/// Parses `argv`, program name included.
pub(crate) fn parse(argv: &[OsString]) -> Result<Parsed, UsageError> {
    let mut args = Args {
        args: argv.get(1..).unwrap_or_default(),
    };
    if args.peek().is_none() {
        return Ok(Parsed::Help(ROOT_HELP));
    }
    let mut env = None;
    while let Some(arg) = args.next_str(ROOT)? {
        let (name, inline) = split_option(arg);
        match name {
            "--version" if inline.is_none() => return Ok(Parsed::Version),
            "--help" if inline.is_none() => return Ok(Parsed::Help(ROOT_HELP)),
            "--env" => {
                let value = args.value("--env", inline, ROOT)?;
                if !crate::status::is_valid_root_env(value) {
                    return Err(
                        ROOT.error("invalid `--env` name (use letters, digits, `-` and `_`)")
                    );
                }
                set_once(&mut env, value.to_owned(), "--env", ROOT)?;
            }
            "hybrid" if env.is_some() => {
                return Err(ROOT.error("`hybrid` takes `--env` after `hybrid start`"));
            }
            "hybrid" => return Ok(Parsed::Hybrid(args.rest())),
            _ if arg.starts_with('-') => {
                return Err(ROOT.error(unknown("option", arg)));
            }
            command => {
                let command = match command {
                    "auth" => match parse_auth(&mut args)? {
                        Ok(auth) => Command::Auth(auth),
                        Err(help) => return Ok(Parsed::Help(help)),
                    },
                    "run" => match parse_run(&mut args)? {
                        Ok(run) => Command::Run(run),
                        Err(help) => return Ok(Parsed::Help(help)),
                    },
                    "top" => match parse_top(&mut args)? {
                        Some(json) => Command::Top { json },
                        None => return Ok(Parsed::Help(TOP_HELP)),
                    },
                    "update" => match parse_update(&mut args)? {
                        Some(request) => Command::Update(Box::new(request)),
                        None => return Ok(Parsed::Help(UPDATE_HELP)),
                    },
                    _ => return Err(ROOT.error(unknown("command", command))),
                };
                return Ok(Parsed::Command { env, command });
            }
        }
    }
    Err(ROOT.error("missing command"))
}

/// `Err(help)` asks for that help text.
type OrHelp<T> = Result<T, &'static str>;

/// A subcommand that takes no arguments: `None` asks for its help.
fn no_arguments(args: &mut Args<'_>, help: Help) -> Result<Option<()>, UsageError> {
    match args.next_str(help)? {
        None => Ok(Some(())),
        Some("--help") => Ok(None),
        Some(arg) => Err(unexpected(help, arg)),
    }
}

fn parse_auth(args: &mut Args<'_>) -> Result<OrHelp<Auth>, UsageError> {
    let (auth, help) = match args.next_str(AUTH)? {
        None | Some("--help") => return Ok(Err(AUTH_HELP)),
        Some("login") => return parse_auth_login(args),
        Some("logout") => (Auth::Logout, AUTH_LOGOUT),
        Some("status") => (Auth::Status, AUTH_STATUS),
        Some("token") => (Auth::Token, AUTH_TOKEN),
        Some(arg) if arg.starts_with('-') => return Err(unexpected(AUTH, arg)),
        Some(arg) => {
            return Err(AUTH.error(unknown("auth command", arg)));
        }
    };
    Ok(no_arguments(args, help)?.map(|()| auth).ok_or(help.text))
}

fn parse_auth_login(args: &mut Args<'_>) -> Result<OrHelp<Auth>, UsageError> {
    let mut no_browser = false;
    let mut paste = false;
    while let Some(arg) = args.next_str(AUTH_LOGIN)? {
        match arg {
            "-b" | "--no-browser" => no_browser = true,
            "-p" | "--paste" => paste = true,
            "--help" => return Ok(Err(AUTH_LOGIN_HELP)),
            _ => return Err(unexpected(AUTH_LOGIN, arg)),
        }
    }
    Ok(Ok(Auth::Login { no_browser, paste }))
}

/// `top [--json]`: `None` asks for its help.
fn parse_top(args: &mut Args<'_>) -> Result<Option<bool>, UsageError> {
    let mut json = false;
    while let Some(arg) = args.next_str(TOP)? {
        match arg {
            "--json" => json = true,
            "--help" => return Ok(None),
            _ => return Err(unexpected(TOP, arg)),
        }
    }
    Ok(Some(json))
}

/// `update [OPTIONS]`, with the hidden `--base-url` and `--install-path`
/// (section 4.2): `None` asks for its help.
fn parse_update(args: &mut Args<'_>) -> Result<Option<UpdateRequest>, UsageError> {
    let mut pinned_version = None;
    let mut base_url = None;
    let mut install_path = None;
    let mut force = false;
    let mut check_only = false;
    let mut dry_run = false;
    while let Some(arg) = args.next_str(UPDATE)? {
        let (name, inline) = split_option(arg);
        match name {
            "--version" => {
                let value = args.value(name, inline, UPDATE)?;
                let version = Version::parse(value).map_err(|_| {
                    UPDATE.error("invalid `--version` (use a release version such as `1.2.3`)")
                })?;
                set_once(&mut pinned_version, version, name, UPDATE)?;
            }
            "--base-url" => {
                let value = args.value(name, inline, UPDATE)?;
                let url = match Url::parse(value) {
                    Ok(url) if matches!(url.scheme(), "http" | "https") => url,
                    _ => {
                        return Err(
                            UPDATE.error("`--base-url` must be an absolute http or https URL")
                        );
                    }
                };
                set_once(&mut base_url, url, name, UPDATE)?;
            }
            "--install-path" => {
                let value = args.value(name, inline, UPDATE)?;
                set_once(&mut install_path, PathBuf::from(value), name, UPDATE)?;
            }
            "--force" if inline.is_none() => force = true,
            "--check" if inline.is_none() => check_only = true,
            "--dry-run" if inline.is_none() => dry_run = true,
            "--help" if inline.is_none() => return Ok(None),
            _ => return Err(unexpected(UPDATE, arg)),
        }
    }
    Ok(Some(UpdateRequest {
        pinned_version,
        force,
        check_only,
        dry_run,
        base_url,
        install_path,
    }))
}

/// The options a run command reads after the harness name (and after a
/// kept subcommand word such as `codex app`).
#[derive(Clone, Copy)]
struct RunGrammar {
    help: Help,
    /// `--help` prints [`Self::help`]. Otherwise it goes to the harness.
    own_help: bool,
    /// The first other argument and everything after it go to the harness.
    /// Otherwise any other argument is an error.
    passthrough: bool,
    /// Reads `--isolated` (`run claude`).
    isolated: bool,
    /// Reads `--model` and `--auth` (`run codex`).
    codex: bool,
}

impl RunGrammar {
    const fn new(help: Help, own_help: bool, passthrough: bool) -> Self {
        Self {
            help,
            own_help,
            passthrough,
            isolated: false,
            codex: false,
        }
    }
}

#[derive(Default)]
struct RunOptions {
    local: bool,
    config: Option<PathBuf>,
    isolated: bool,
    codex: CodexOptions,
    /// What goes to the harness.
    args: Vec<OsString>,
}

/// The harness a run launches, before its options are read.
enum Target {
    Claude,
    ClaudeDesktop(AppAction),
    Codex,
    CodexApp,
    CodexConfigure,
    Harness(Harness),
}

fn parse_run(args: &mut Args<'_>) -> Result<OrHelp<Run>, UsageError> {
    let harness = match args.next_str(RUN)? {
        None => return Err(RUN.error("missing harness")),
        Some("--help") => return Ok(Err(RUN_HELP)),
        Some(arg) if arg.starts_with('-') => {
            return Err(RUN.error(format!(
                "the harness comes first, before {}: `rayline run <HARNESS> [--local | --config <file>]`",
                shown(arg)
            )));
        }
        Some(harness) => harness,
    };
    let codex = |help, own_help, passthrough| RunGrammar {
        codex: true,
        ..RunGrammar::new(help, own_help, passthrough)
    };
    let (target, grammar) = match harness {
        "claude" => (
            Target::Claude,
            RunGrammar {
                isolated: true,
                ..RunGrammar::new(RUN, false, true)
            },
        ),
        "claude-desktop" => {
            let action = match args.peek().and_then(|arg| arg.to_str()) {
                Some(word) if !word.starts_with('-') => {
                    args.next();
                    AppAction::parse(word).ok_or_else(|| {
                        CLAUDE_DESKTOP.error(unknown("claude-desktop command", word))
                    })?
                }
                _ => AppAction::Start,
            };
            (
                Target::ClaudeDesktop(action),
                RunGrammar::new(CLAUDE_DESKTOP, true, false),
            )
        }
        "codex" => match args.peek().and_then(|arg| arg.to_str()) {
            Some("app") => {
                args.next();
                (Target::CodexApp, codex(CODEX_APP, true, true))
            }
            Some("configure") => {
                args.next();
                (Target::CodexConfigure, codex(CODEX_CONFIGURE, true, false))
            }
            _ => (Target::Codex, codex(RUN, false, true)),
        },
        name => match Harness::from_command(name) {
            Some(harness) => (Target::Harness(harness), RunGrammar::new(RUN, false, true)),
            None => return Err(RUN.error(unknown("harness", name))),
        },
    };
    let Some(options) = run_options(args, grammar)? else {
        return Ok(Err(grammar.help.text));
    };
    let config = match (options.local, options.config) {
        (true, _) => ConfigFlag::Local,
        (false, Some(file)) => ConfigFlag::File(file),
        (false, None) => ConfigFlag::Default,
    };
    let launch = match target {
        Target::Claude => Launch::Claude {
            isolated: options.isolated,
            args: options.args,
        },
        Target::ClaudeDesktop(action) => Launch::ClaudeDesktop(action),
        Target::Codex => Launch::Codex {
            options: options.codex,
            args: options.args,
        },
        Target::CodexApp => Launch::CodexApp {
            options: options.codex,
            args: options.args,
        },
        Target::CodexConfigure => Launch::CodexConfigure(options.codex),
        Target::Harness(harness) => Launch::Harness {
            harness,
            args: options.args,
        },
    };
    Ok(Ok(Run { config, launch }))
}

/// Reads the Rayline options of a run, which come first (section 3.2): up to
/// the first other argument, or `--`, after which the rest goes to the
/// harness. `None` asks for the command's own help.
fn run_options(args: &mut Args<'_>, grammar: RunGrammar) -> Result<Option<RunOptions>, UsageError> {
    let help = grammar.help;
    let mut options = RunOptions::default();
    while let Some(arg) = args.peek() {
        let Some(text) = arg.to_str() else {
            if grammar.passthrough {
                break;
            }
            return Err(help.error("got an argument that is not valid UTF-8"));
        };
        let (name, inline) = split_option(text);
        let flag = inline.is_none();
        match name {
            "--" if flag && grammar.passthrough => {
                args.next();
                break;
            }
            "--local" if flag => {
                args.next();
                if options.config.is_some() {
                    return Err(both_config_flags(help));
                }
                if options.local {
                    return Err(help.error("`--local` given more than once"));
                }
                options.local = true;
            }
            "--config" => {
                args.next();
                if options.local {
                    return Err(both_config_flags(help));
                }
                let file = args.value(name, inline, help)?;
                set_once(&mut options.config, PathBuf::from(file), name, help)?;
            }
            "--isolated" if flag && grammar.isolated => {
                args.next();
                if options.isolated {
                    return Err(help.error("`--isolated` given more than once"));
                }
                options.isolated = true;
            }
            "--model" if grammar.codex => {
                args.next();
                let model = args.value(name, inline, help)?;
                set_once(&mut options.codex.model, model.to_owned(), name, help)?;
            }
            "--auth" if grammar.codex => {
                args.next();
                let value = args.value(name, inline, help)?;
                let mode = CodexAuthMode::parse(value)
                    .ok_or_else(|| help.error("`--auth` must be auto, subscription or none"))?;
                set_once(&mut options.codex.auth, mode, name, help)?;
            }
            "--help" if flag && grammar.own_help => return Ok(None),
            _ if grammar.passthrough => break,
            _ => return Err(unexpected(help, text)),
        }
    }
    if grammar.passthrough {
        options.args = args.rest();
    }
    Ok(Some(options))
}

fn both_config_flags(help: Help) -> UsageError {
    help.error("`--local` and `--config` can't be used together")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        std::iter::once("rayline")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn parsed(args: &[&str]) -> Parsed {
        parse(&argv(args)).unwrap_or_else(|error| panic!("{args:?} was rejected: {error}"))
    }

    fn command(args: &[&str]) -> (Option<String>, Command) {
        match parsed(args) {
            Parsed::Command { env, command } => (env, command),
            other => panic!("{args:?} gave {other:?}"),
        }
    }

    fn run(args: &[&str]) -> Run {
        match command(args) {
            (None, Command::Run(run)) => run,
            other => panic!("{args:?} gave {other:?}"),
        }
    }

    fn error(args: &[&str]) -> String {
        match parse(&argv(args)) {
            Err(error) => error.to_string(),
            Ok(parsed) => panic!("{args:?} was accepted as {parsed:?}"),
        }
    }

    fn assert_error(args: &[&str], problem: &str) {
        let error = error(args);
        assert!(
            error.starts_with(&format!("rayline: {problem}\n\n")),
            "{args:?}: {error}"
        );
    }

    fn claude(args: &[&str]) -> Launch {
        Launch::Claude {
            isolated: false,
            args: os(args),
        }
    }

    fn harness(harness: Harness, args: &[&str]) -> Launch {
        Launch::Harness {
            harness,
            args: os(args),
        }
    }

    // ── Help text (section 3) ────────────────────────────────────────────

    /// The code block that opens section 3 of the spec.
    fn spec_section_3_block() -> String {
        let spec = include_str!("../../../docs/spec.md").replace("\r\n", "\n");
        let section = spec
            .split_once("\n## 3. Public CLI\n")
            .expect("section 3")
            .1;
        let block = section
            .split_once("```\n")
            .expect("block start")
            .1
            .split_once("```\n")
            .expect("block end")
            .0;
        block.to_owned()
    }

    #[test]
    fn root_and_run_help_are_section_3_word_for_word() {
        assert_eq!(format!("{ROOT_HELP}\n{RUN_HELP}"), spec_section_3_block());
    }

    #[test]
    fn root_help_is_printed_with_no_arguments_and_for_help() {
        assert_eq!(parsed(&[]), Parsed::Help(ROOT_HELP));
        assert_eq!(parsed(&["--help"]), Parsed::Help(ROOT_HELP));
        assert_eq!(parsed(&["--env", "dev", "--help"]), Parsed::Help(ROOT_HELP));
    }

    #[test]
    fn run_help_is_the_run_usage_and_harnesses() {
        assert_eq!(parsed(&["run", "--help"]), Parsed::Help(RUN_HELP));
    }

    #[test]
    fn version_is_a_root_option() {
        assert_eq!(parsed(&["--version"]), Parsed::Version);
        assert_eq!(parsed(&["--env", "dev", "--version"]), Parsed::Version);
    }

    #[test]
    fn kept_commands_print_their_own_help() {
        for (args, help) in [
            (&["auth"][..], AUTH_HELP),
            (&["auth", "--help"], AUTH_HELP),
            (&["auth", "login", "--help"], AUTH_LOGIN_HELP),
            (&["auth", "logout", "--help"], AUTH_LOGOUT_HELP),
            (&["auth", "status", "--help"], AUTH_STATUS_HELP),
            (&["auth", "token", "--help"], AUTH_TOKEN_HELP),
            (&["top", "--help"], TOP_HELP),
            (&["update", "--help"], UPDATE_HELP),
            (&["run", "claude-desktop", "--help"], CLAUDE_DESKTOP_HELP),
            (
                &["run", "claude-desktop", "status", "--help"],
                CLAUDE_DESKTOP_HELP,
            ),
            (&["run", "codex", "app", "--help"], CODEX_APP_HELP),
            (
                &["run", "codex", "configure", "--help"],
                CODEX_CONFIGURE_HELP,
            ),
        ] {
            assert_eq!(parsed(args), Parsed::Help(help), "{args:?}");
        }
    }

    #[test]
    fn every_help_names_rayline_and_no_removed_option() {
        for help in [
            ROOT_HELP,
            RUN_HELP,
            AUTH_HELP,
            AUTH_LOGIN_HELP,
            AUTH_LOGOUT_HELP,
            AUTH_STATUS_HELP,
            AUTH_TOKEN_HELP,
            CLAUDE_DESKTOP_HELP,
            CODEX_APP_HELP,
            CODEX_CONFIGURE_HELP,
            TOP_HELP,
            UPDATE_HELP,
        ] {
            assert!(help.starts_with("Usage: rayline "), "{help}");
            for removed in [
                "raydock",
                "--auth-token",
                "--env",
                "--base-url",
                "--install-path",
                "--subscription",
                "hybrid run",
            ] {
                assert!(!help.contains(removed), "{removed} in {help}");
            }
        }
    }

    #[test]
    fn a_usage_error_names_the_problem_the_usage_and_the_help() {
        assert_eq!(
            error(&["run", "nope"]),
            "rayline: unknown harness `nope`\n\n\
             Usage: rayline run <HARNESS> [--local | --config <file>] [HARNESS ARGS]...\n\
             Run `rayline run --help` for details."
        );
    }

    // ── run <HARNESS> (section 3.2) ──────────────────────────────────────

    #[test]
    fn every_harness_runs_in_cloud_mode_with_neither_flag() {
        for (name, launch) in [
            ("claude", claude(&[])),
            ("claude-desktop", Launch::ClaudeDesktop(AppAction::Start)),
            (
                "codex",
                Launch::Codex {
                    options: CodexOptions::default(),
                    args: Vec::new(),
                },
            ),
            ("opencode", harness(Harness::OpenCode, &[])),
            ("pi", harness(Harness::Pi, &[])),
            ("omp", harness(Harness::Omp, &[])),
            ("hermes", harness(Harness::Hermes, &[])),
            ("openclaw", harness(Harness::OpenClaw, &[])),
        ] {
            assert_eq!(
                run(&["run", name]),
                Run {
                    config: ConfigFlag::Default,
                    launch
                },
                "{name}"
            );
        }
    }

    #[test]
    fn the_harness_list_in_run_help_is_every_harness_run_takes() {
        let names = RUN_HELP
            .split_once("Harnesses:\n")
            .expect("harnesses")
            .1
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 8);
        for name in names {
            run(&["run", name]);
        }
    }

    #[test]
    fn local_and_config_select_the_config() {
        assert_eq!(run(&["run", "pi", "--local"]).config, ConfigFlag::Local);
        assert_eq!(
            run(&["run", "pi", "--config", "my.json"]).config,
            ConfigFlag::File(PathBuf::from("my.json"))
        );
        assert_eq!(
            run(&["run", "pi", "--config=my.json"]).config,
            ConfigFlag::File(PathBuf::from("my.json"))
        );
    }

    #[test]
    fn local_and_config_together_are_a_usage_error() {
        for args in [
            &["run", "claude", "--local", "--config", "x.json"][..],
            &["run", "claude", "--config", "x.json", "--local"],
            &["run", "codex", "--config=x.json", "--local"],
            &["run", "codex", "app", "--local", "--config", "x.json"],
            &["run", "claude-desktop", "--config", "x.json", "--local"],
        ] {
            assert_error(args, "`--local` and `--config` can't be used together");
        }
    }

    #[test]
    fn a_flag_given_twice_is_a_usage_error() {
        assert_error(
            &["run", "pi", "--local", "--local"],
            "`--local` given more than once",
        );
        assert_error(
            &["run", "pi", "--config", "a", "--config", "b"],
            "`--config` given more than once",
        );
        assert_error(
            &["run", "claude", "--isolated", "--isolated"],
            "`--isolated` given more than once",
        );
        assert_error(
            &["run", "codex", "--model", "a", "--model", "b"],
            "`--model` given more than once",
        );
    }

    #[test]
    fn config_needs_a_value() {
        for args in [
            &["run", "pi", "--config"][..],
            &["run", "pi", "--config="],
            &["run", "pi", "--config", "--local"],
        ] {
            assert_error(args, "`--config` needs a value");
        }
    }

    #[test]
    fn an_unknown_or_missing_harness_is_a_usage_error() {
        assert_error(&["run"], "missing harness");
        assert_error(&["run", "nope"], "unknown harness `nope`");
        assert_error(&["run", "Claude"], "unknown harness");
        assert_error(&["run", "claude-code"], "unknown harness `claude-code`");
    }

    #[test]
    fn the_harness_comes_before_rayline_options() {
        for args in [
            &["run", "--local", "claude"][..],
            &["run", "--config", "x.json", "pi"],
        ] {
            let error = error(args);
            assert!(
                error.starts_with("rayline: the harness comes first"),
                "{error}"
            );
        }
    }

    #[test]
    fn the_first_other_argument_and_everything_after_go_to_the_harness() {
        assert_eq!(
            run(&["run", "claude", "--local", "-p", "hi", "--config", "x.json"]),
            Run {
                config: ConfigFlag::Local,
                launch: claude(&["-p", "hi", "--config", "x.json"]),
            }
        );
        assert_eq!(
            run(&["run", "pi", "--model", "m", "--local"]).launch,
            harness(Harness::Pi, &["--model", "m", "--local"])
        );
        assert_eq!(
            run(&["run", "claude", "--model", "opus", "--isolated"]).launch,
            claude(&["--model", "opus", "--isolated"])
        );
    }

    #[test]
    fn everything_after_double_dash_goes_to_the_harness() {
        assert_eq!(
            run(&["run", "claude", "--local", "--", "--config", "x", "--"]),
            Run {
                config: ConfigFlag::Local,
                launch: claude(&["--config", "x", "--"]),
            }
        );
        assert_eq!(
            run(&["run", "hermes", "--", "--local"]),
            Run {
                config: ConfigFlag::Default,
                launch: harness(Harness::Hermes, &["--local"]),
            }
        );
        assert_eq!(
            run(&["run", "codex", "--", "app"]).launch,
            Launch::Codex {
                options: CodexOptions::default(),
                args: os(&["app"]),
            }
        );
    }

    #[test]
    fn harness_help_goes_to_the_harness() {
        for name in ["claude", "opencode", "pi", "omp", "hermes", "openclaw"] {
            let launch = run(&["run", name, "--help"]).launch;
            let args = match launch {
                Launch::Claude { args, .. } | Launch::Harness { args, .. } => args,
                other => panic!("{name}: {other:?}"),
            };
            assert_eq!(args, os(&["--help"]), "{name}");
        }
        assert_eq!(
            run(&["run", "codex", "--local", "--help"]),
            Run {
                config: ConfigFlag::Local,
                launch: Launch::Codex {
                    options: CodexOptions::default(),
                    args: os(&["--help"]),
                },
            }
        );
        assert_eq!(
            run(&["run", "claude", "--", "--help"]).launch,
            claude(&["--help"])
        );
    }

    #[test]
    fn env_is_global_and_comes_before_the_command() {
        assert_eq!(
            command(&["--env", "dev", "run", "claude"]),
            (
                Some("dev".to_owned()),
                Command::Run(Run {
                    config: ConfigFlag::Default,
                    launch: claude(&[]),
                })
            )
        );
        assert_eq!(command(&["--env=dev", "top"]).0, Some("dev".to_owned()));
        // After the harness it is the harness's own option.
        assert_eq!(
            run(&["run", "pi", "--env", "dev"]).launch,
            harness(Harness::Pi, &["--env", "dev"])
        );
        for args in [
            &["auth", "--env", "dev"][..],
            &["auth", "login", "--env", "dev"],
            &["auth", "status", "--env", "dev"],
            &["run", "claude-desktop", "--env=dev"],
            &["run", "codex", "configure", "--env", "dev"],
            &["top", "--env", "dev"],
            &["update", "--env=dev"],
        ] {
            assert!(
                error(args).contains("`--env` goes before the command: `rayline --env <name> "),
                "{args:?}"
            );
        }
    }

    #[test]
    fn env_names_are_checked() {
        assert_error(
            &["--env", "a/b", "run", "pi"],
            "invalid `--env` name (use letters, digits, `-` and `_`)",
        );
        for args in [&["--env"][..], &["--env="], &["--env", "--help"]] {
            assert_error(args, "`--env` needs a value");
        }
        assert_error(
            &["--env", "a", "--env", "b", "top"],
            "`--env` given more than once",
        );
        assert_error(&["--env", "dev"], "missing command");
    }

    // ── Kept options and subcommands (section 9) ─────────────────────────

    #[test]
    fn claude_takes_isolated() {
        assert_eq!(
            run(&["run", "claude", "--isolated", "-p", "hi"]).launch,
            Launch::Claude {
                isolated: true,
                args: os(&["-p", "hi"]),
            }
        );
        assert_eq!(
            run(&["run", "pi", "--isolated"]).launch,
            harness(Harness::Pi, &["--isolated"])
        );
    }

    #[test]
    fn claude_desktop_takes_a_command_and_no_other_arguments() {
        for (word, action) in [
            ("start", AppAction::Start),
            ("status", AppAction::Status),
            ("validate", AppAction::Validate),
            ("stop", AppAction::Stop),
            ("install-service", AppAction::InstallService),
            ("uninstall-service", AppAction::UninstallService),
            ("off", AppAction::Off),
            ("uninstall", AppAction::Uninstall),
        ] {
            assert_eq!(
                run(&["run", "claude-desktop", word]).launch,
                Launch::ClaudeDesktop(action)
            );
        }
        assert_eq!(
            run(&["run", "claude-desktop", "status", "--config", "x.json"]).config,
            ConfigFlag::File(PathBuf::from("x.json"))
        );
        assert_error(
            &["run", "claude-desktop", "bogus"],
            "unknown claude-desktop command `bogus`",
        );
        for removed in ["--manual", "--prefixed-ids", "--desktop-config"] {
            assert_error(
                &["run", "claude-desktop", removed],
                &format!("unexpected `{removed}`"),
            );
        }
        assert_error(
            &["run", "claude-desktop", "status", "extra"],
            "unexpected `extra`",
        );
    }

    #[test]
    fn codex_takes_model_and_auth_before_its_own_arguments() {
        assert_eq!(
            run(&[
                "run",
                "codex",
                "--model",
                "m",
                "--auth=none",
                "exec",
                "--model",
                "x"
            ])
            .launch,
            Launch::Codex {
                options: CodexOptions {
                    model: Some("m".to_owned()),
                    auth: Some(CodexAuthMode::None),
                },
                args: os(&["exec", "--model", "x"]),
            }
        );
    }

    #[test]
    fn codex_auth_takes_only_auto_subscription_and_none() {
        for (value, mode) in [
            ("auto", CodexAuthMode::Auto),
            ("subscription", CodexAuthMode::Subscription),
            ("none", CodexAuthMode::None),
        ] {
            assert_eq!(
                run(&["run", "codex", "--auth", value]).launch,
                Launch::Codex {
                    options: CodexOptions {
                        model: None,
                        auth: Some(mode),
                    },
                    args: Vec::new(),
                }
            );
        }
        for alias in ["no-auth", "local", "codex-subscription", "chatgpt", "codex"] {
            assert_error(
                &["run", "codex", "--auth", alias],
                "`--auth` must be auto, subscription or none",
            );
        }
    }

    #[test]
    fn codex_app_takes_its_options_then_the_workspace() {
        assert_eq!(
            run(&["run", "codex", "app", "--model", "m", "~/work"]),
            Run {
                config: ConfigFlag::Default,
                launch: Launch::CodexApp {
                    options: CodexOptions {
                        model: Some("m".to_owned()),
                        auth: None,
                    },
                    args: os(&["~/work"]),
                },
            }
        );
        // With a Rayline option first, `app` is Codex CLI's own argument.
        assert_eq!(
            run(&["run", "codex", "--local", "app"]),
            Run {
                config: ConfigFlag::Local,
                launch: Launch::Codex {
                    options: CodexOptions::default(),
                    args: os(&["app"]),
                },
            }
        );
    }

    #[test]
    fn codex_configure_takes_only_model_and_auth() {
        assert_eq!(
            run(&["run", "codex", "configure", "--auth", "auto"]).launch,
            Launch::CodexConfigure(CodexOptions {
                model: None,
                auth: Some(CodexAuthMode::Auto),
            })
        );
        assert_error(
            &["run", "codex", "configure", "--base-url", "http://x"],
            "unexpected `--base-url`",
        );
        assert_error(
            &["run", "codex", "configure", "extra"],
            "unexpected `extra`",
        );
    }

    #[test]
    fn local_mode_names_the_kept_options_it_does_not_run_yet() {
        let local = |args: &[&str]| run(args).not_in_local_mode();
        assert_eq!(
            local(&["run", "pi", "--config", "routing.json"]),
            Some("`--config`")
        );
        assert_eq!(
            local(&["run", "claude", "--isolated"]),
            Some("`--isolated`")
        );
        assert_eq!(
            local(&["run", "claude-desktop"]),
            Some("`run claude-desktop`")
        );
        assert_eq!(local(&["run", "codex", "app"]), Some("`run codex app`"));
        assert_eq!(
            local(&["run", "codex", "configure"]),
            Some("`run codex configure`")
        );
        assert_eq!(local(&["run", "codex", "--model", "m"]), Some("`--model`"));
        assert_eq!(local(&["run", "codex", "--auth", "auto"]), Some("`--auth`"));
        for args in [
            &["run", "claude", "-p", "hi"][..],
            &["run", "codex", "exec"],
            &["run", "pi"],
        ] {
            assert_eq!(local(args), None, "{args:?}");
        }
    }

    #[test]
    fn each_launch_names_its_protocol() {
        let protocol = |args: &[&str]| run(args).launch.protocol();
        assert_eq!(protocol(&["run", "claude"]), Protocol::AnthropicMessages);
        assert_eq!(
            protocol(&["run", "claude-desktop"]),
            Protocol::AnthropicMessages
        );
        for codex in [&["run", "codex"][..], &["run", "codex", "app"]] {
            assert_eq!(protocol(codex), Protocol::OpenaiResponses);
        }
        for name in ["opencode", "pi", "omp"] {
            assert_eq!(
                protocol(&["run", name]),
                Protocol::AnthropicMessages,
                "{name}"
            );
        }
        assert_eq!(protocol(&["run", "hermes"]), Protocol::OpenaiResponses);
        assert_eq!(protocol(&["run", "openclaw"]), Protocol::OpenaiChat);
    }

    // ── auth, top, update ────────────────────────────────────────────────

    #[test]
    fn auth_commands_parse() {
        assert_eq!(
            command(&["auth", "login", "-b", "--paste"]).1,
            Command::Auth(Auth::Login {
                no_browser: true,
                paste: true,
            })
        );
        assert_eq!(command(&["auth", "logout"]).1, Command::Auth(Auth::Logout));
        assert_eq!(command(&["auth", "status"]).1, Command::Auth(Auth::Status));
        assert_eq!(command(&["auth", "token"]).1, Command::Auth(Auth::Token));
        assert_error(&["auth", "whoami"], "unknown auth command `whoami`");
        assert_error(&["auth", "token", "--json"], "unexpected `--json`");
        assert_error(
            &["auth", "login", "--auth-token", "t"],
            "unexpected `--auth-token`",
        );
    }

    #[test]
    fn top_takes_only_json() {
        assert_eq!(command(&["top"]).1, Command::Top { json: false });
        assert_eq!(command(&["top", "--json"]).1, Command::Top { json: true });
        assert_error(&["top", "--watch"], "unexpected `--watch`");
    }

    #[test]
    fn update_takes_its_options_and_the_hidden_release_testing_ones() {
        assert_eq!(
            command(&[
                "update",
                "--check",
                "--version",
                "1.2.3",
                "--base-url=http://127.0.0.1:1",
                "--install-path",
                "/tmp/raydock",
            ])
            .1,
            Command::Update(Box::new(UpdateRequest {
                pinned_version: Some(Version::parse("1.2.3").unwrap()),
                force: false,
                check_only: true,
                dry_run: false,
                base_url: Some(Url::parse("http://127.0.0.1:1").unwrap()),
                install_path: Some(PathBuf::from("/tmp/raydock")),
            }))
        );
        assert_error(&["update", "--channel", "dev"], "unexpected `--channel`");
    }

    #[test]
    fn update_refuses_a_bad_version_or_base_url_at_parse() {
        for version in ["latest", "1.2.3+", "v1..2"] {
            assert_error(
                &["update", "--version", version],
                "invalid `--version` (use a release version such as `1.2.3`)",
            );
        }
        for url in [
            "get.rayline.ai",
            "/releases",
            "ftp://get.rayline.ai",
            "file:///tmp/x",
        ] {
            assert_error(
                &["update", &format!("--base-url={url}")],
                "`--base-url` must be an absolute http or https URL",
            );
        }
    }

    #[test]
    fn update_options_need_a_value_and_are_given_once() {
        for (name, value) in [
            ("--version", "1.2.3"),
            ("--base-url", "http://127.0.0.1:1"),
            ("--install-path", "/tmp/raydock"),
        ] {
            let needs = format!("`{name}` needs a value");
            assert_error(&["update", name], &needs);
            assert_error(&["update", &format!("{name}=")], &needs);
            assert_error(&["update", name, "--check"], &needs);
            assert_error(
                &["update", name, value, &format!("{name}={value}")],
                &format!("`{name}` given more than once"),
            );
        }
        for flag in ["--force", "--check", "--dry-run", "--help"] {
            assert_error(
                &["update", &format!("{flag}=yes")],
                &format!("unexpected `{flag}=…`"),
            );
        }
        assert_error(&["update", "now"], "unexpected `now`");
    }

    #[test]
    fn codex_options_need_a_value_and_are_given_once() {
        for words in [
            &["run", "codex"][..],
            &["run", "codex", "app"],
            &["run", "codex", "configure"],
        ] {
            let with = |extra: &[&str]| -> Vec<String> {
                words
                    .iter()
                    .chain(extra)
                    .map(|arg| (*arg).to_owned())
                    .collect()
            };
            let check = |extra: &[&str], problem: &str| {
                let args = with(extra);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                assert_error(&args, problem);
            };
            for name in ["--model", "--auth"] {
                let needs = format!("`{name}` needs a value");
                check(&[name], &needs);
                check(&[&format!("{name}=")], &needs);
                check(&[name, "--local"], &needs);
            }
            check(
                &["--model", "a", "--model=b"],
                "`--model` given more than once",
            );
            check(
                &["--auth", "auto", "--auth=none"],
                "`--auth` given more than once",
            );
        }
    }

    #[test]
    fn an_unknown_auth_option_or_extra_argument_is_a_usage_error() {
        assert_error(&["auth", "--bogus"], "unexpected `--bogus`");
        assert_error(&["auth", "login", "now"], "unexpected `now`");
        assert_error(&["auth", "logout", "--all"], "unexpected `--all`");
        assert_error(&["auth", "status", "dev"], "unexpected `dev`");
    }

    #[test]
    fn a_flag_given_a_value_is_a_usage_error_where_nothing_follows() {
        assert_error(
            &["run", "claude-desktop", "--local=yes"],
            "unexpected `--local=…`",
        );
        assert_error(
            &["run", "codex", "configure", "--help=yes"],
            "unexpected `--help=…`",
        );
        assert_error(&["top", "--json=yes"], "unexpected `--json=…`");
    }

    /// An argument that is not valid UTF-8 on this platform.
    fn not_utf8() -> OsString {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            OsString::from_vec(vec![0xff])
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt as _;
            OsString::from_wide(&[0xd800])
        }
    }

    /// `args` with a non-UTF-8 argument after them.
    fn then_not_utf8(args: &[&str]) -> Vec<OsString> {
        let mut argv = argv(args);
        argv.push(not_utf8());
        argv
    }

    #[test]
    fn an_argument_that_is_not_utf8_is_a_usage_error() {
        for (args, help) in [
            (&[][..], "rayline"),
            (&["--env", "dev"], "rayline"),
            (&["auth"], "rayline auth"),
            (&["auth", "login"], "rayline auth login"),
            (&["auth", "logout"], "rayline auth logout"),
            (&["auth", "status"], "rayline auth status"),
            (&["auth", "token"], "rayline auth token"),
            (&["top"], "rayline top"),
            (&["update"], "rayline update"),
            (&["run"], "rayline run"),
            (&["run", "claude-desktop"], "rayline run claude-desktop"),
            (
                &["run", "claude-desktop", "stop"],
                "rayline run claude-desktop",
            ),
            (
                &["run", "codex", "configure"],
                "rayline run codex configure",
            ),
        ] {
            let error = match parse(&then_not_utf8(args)) {
                Err(error) => error.to_string(),
                Ok(parsed) => panic!("{args:?} was accepted as {parsed:?}"),
            };
            assert!(
                error.starts_with(
                    "rayline: got an argument that is not valid UTF-8

"
                ) && error.ends_with(&format!("Run `{help} --help` for details.")),
                "{args:?}: {error}"
            );
        }
    }

    #[test]
    fn an_argument_that_is_not_utf8_goes_to_a_harness_unchanged() {
        assert_eq!(
            parse(&then_not_utf8(&["run", "claude", "--local"])),
            Ok(Parsed::Command {
                env: None,
                command: Command::Run(Run {
                    config: ConfigFlag::Local,
                    launch: Launch::Claude {
                        isolated: false,
                        args: vec![not_utf8()],
                    },
                }),
            })
        );
        assert_eq!(
            parse(&then_not_utf8(&["run", "pi", "--"])),
            Ok(Parsed::Command {
                env: None,
                command: Command::Run(Run {
                    config: ConfigFlag::Default,
                    launch: Launch::Harness {
                        harness: Harness::Pi,
                        args: vec![not_utf8()],
                    },
                }),
            })
        );
    }

    // ── Removed forms (section 10.1) ─────────────────────────────────────

    #[test]
    fn removed_commands_are_unknown() {
        for name in [
            "claude",
            "claude-desktop",
            "codex",
            "opencode",
            "pi",
            "omp",
            "hermes",
            "openclaw",
            "status",
            "local",
            "router",
            "chat",
            "api",
            "menubar",
            "install-cli",
            "hook-send",
            "agent",
        ] {
            assert_error(&[name], &format!("unknown command `{name}`"));
        }
        assert_error(&["claude", "run"], "unknown command `claude`");
    }

    #[test]
    fn removed_root_options_are_unknown() {
        assert_error(
            &["--auth-token", "t", "run", "pi"],
            "unknown option `--auth-token`",
        );
        assert_error(
            &["--auth-token=t", "top"],
            "unknown option `--auth-token=…`",
        );
        assert_error(&["-v"], "unknown option `-v`");
    }

    #[test]
    fn rejected_arguments_are_not_echoed() {
        let word = "sk-rayline-UNECHOED";
        for args in [
            &[word][..],
            &["run", word],
            &["auth", word],
            &["top", word],
            &["run", "claude-desktop", "status", word],
            &[&format!("--{word}")],
        ] {
            assert!(!error(args).contains(word), "{args:?}");
        }
    }

    #[test]
    fn hybrid_is_hidden_and_takes_its_own_arguments() {
        assert_eq!(
            parsed(&["hybrid", "status"]),
            Parsed::Hybrid(os(&["status"]))
        );
        assert!(
            !ROOT_HELP
                .lines()
                .any(|line| line.trim_start().starts_with("hybrid"))
        );
        assert_error(
            &["--env", "dev", "hybrid", "status"],
            "`hybrid` takes `--env` after `hybrid start`",
        );
    }
}
