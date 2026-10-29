//! The `csub` command line, shared by the `csub` binary and
//! `rayline subscriptions`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::cli::SubscriptionCommand;
use crate::serve::{DEFAULT_PORT, EXIT_POOL_OWNED, ServeError, ServeOptions};
use crate::service::ServiceOptions;

/// Pool several Claude subscriptions behind one local gateway.
#[derive(Parser)]
#[command(name = "csub", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register a Claude config dir as a pool account.
    Add {
        account_id: String,
        #[arg(long)]
        claude_config_dir: PathBuf,
        #[arg(long)]
        control_config_dir: Option<PathBuf>,
        #[command(flatten)]
        pool: PoolArgs,
    },
    /// Remove a pool account from the registry.
    Remove {
        account_id: String,
        #[command(flatten)]
        pool: PoolArgs,
    },
    /// List registered pools and accounts.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Show allowance and placement status.
    Status {
        #[command(flatten)]
        pool: PoolArgs,
        #[arg(long)]
        json: bool,
        #[arg(long, short)]
        verbose: bool,
        /// Only read the running daemon; fail if none is reachable.
        #[arg(long, conflicts_with = "poll_usage")]
        live_only: bool,
        /// Without a daemon, poll usage read-only with existing credentials.
        #[arg(long)]
        poll_usage: bool,
    },
    /// Make the running daemon reread every credential source.
    Reload {
        #[command(flatten)]
        pool: PoolArgs,
    },
    /// Own the pool: status API and gateway on one loopback port.
    Serve {
        #[command(flatten)]
        pool: PoolArgs,
        #[arg(long, env = "CSUB_PORT", default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long, env = "CSUB_STATE_DIR")]
        state_dir: Option<PathBuf>,
        #[arg(
            long,
            env = "CSUB_ANTHROPIC_BASE_URL",
            default_value = "https://api.anthropic.com",
            hide = true
        )]
        anthropic_base_url: String,
    },
    /// Manage the launchd job that runs `csub serve`.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Run Claude Code through the running pool's gateway.
    Claude {
        #[arg(long, default_value = "default")]
        pool: String,
        #[arg(long, env = "CSUB_PORT", default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Arguments passed to `claude` unchanged.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install {
        #[command(flatten)]
        pool: PoolArgs,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    Uninstall,
    Status,
}

#[derive(clap::Args)]
struct PoolArgs {
    #[arg(long, default_value = "default")]
    pool: String,
    /// Registry path (default ~/.config/rayline/subscriptions.json).
    #[arg(long)]
    config: Option<PathBuf>,
}

/// How a front end mounts this command line.
pub struct Frontend {
    /// Shown in help and errors, e.g. `csub` or `raydock subscriptions`.
    pub name: &'static str,
    /// Arguments that reach this command line from the running binary, used
    /// when a launchd job must re-run it (`[]` for csub, `["subscriptions"]`
    /// for raydock).
    pub command_prefix: &'static [&'static str],
}

pub const CSUB: Frontend = Frontend {
    name: "csub",
    command_prefix: &[],
};

/// Parses `argv` (program name first) and runs the command.
pub async fn run<I, T>(frontend: &Frontend, argv: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    use clap::{CommandFactory as _, FromArgMatches as _};
    let parsed = Cli::command()
        .name(frontend.name)
        .bin_name(frontend.name)
        .try_get_matches_from(argv)
        .and_then(|matches| Cli::from_arg_matches(&matches));
    let cli = match parsed {
        Ok(cli) => cli,
        Err(error) => {
            // Help and --version print here and exit 0; usage errors exit 2.
            let _ = error.print();
            return ExitCode::from(error.exit_code().clamp(0, 255) as u8);
        }
    };
    run_command(frontend, cli.command).await
}

async fn run_command(frontend: &Frontend, command: Command) -> ExitCode {
    let registry_command = match command {
        Command::Claude { pool, port, args } => {
            return match crate::launch::claude(&pool, port, args).await {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("csub claude: {error:#}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Serve {
            pool,
            port,
            state_dir,
            anthropic_base_url,
        } => {
            return match crate::serve::serve(ServeOptions {
                pool_id: pool.pool,
                config_path: pool.config,
                port,
                state_dir,
                anthropic_base_url,
            })
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(ServeError::PoolOwned(message)) => {
                    eprintln!("csub serve: {message}");
                    ExitCode::from(EXIT_POOL_OWNED as u8)
                }
                Err(ServeError::Other(error)) => {
                    eprintln!("csub serve: {error:#}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Service { action } => {
            let result = match action {
                ServiceAction::Install { pool, port } => crate::service::install(&ServiceOptions {
                    pool_id: pool.pool,
                    config_path: pool.config,
                    port,
                    command_prefix: frontend
                        .command_prefix
                        .iter()
                        .map(|argument| (*argument).to_owned())
                        .collect(),
                }),
                ServiceAction::Uninstall => crate::service::uninstall(),
                ServiceAction::Status => crate::service::status(),
            };
            return match result {
                Ok(output) => {
                    print!("{output}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("csub service: {error:#}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Add {
            account_id,
            claude_config_dir,
            control_config_dir,
            pool,
        } => SubscriptionCommand::Add {
            account_id,
            pool_id: pool.pool,
            config_path: pool.config,
            claude_config_dir,
            control_config_dir,
        },
        Command::Remove { account_id, pool } => SubscriptionCommand::Remove {
            account_id,
            pool_id: pool.pool,
            config_path: pool.config,
        },
        Command::List { config, json } => SubscriptionCommand::List {
            config_path: config,
            json,
        },
        Command::Status {
            pool,
            json,
            verbose,
            live_only,
            poll_usage,
        } => SubscriptionCommand::Status {
            pool_id: pool.pool,
            config_path: pool.config,
            json,
            verbose,
            live_only,
            poll_usage,
        },
        Command::Reload { pool } => SubscriptionCommand::Reload {
            pool_id: pool.pool,
            config_path: pool.config,
        },
    };
    match crate::cli::run(&registry_command).await {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("csub: {error}");
            ExitCode::FAILURE
        }
    }
}
