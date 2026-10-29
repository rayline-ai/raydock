use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use csub::cli::SubscriptionCommand;
use csub::serve::{DEFAULT_PORT, EXIT_POOL_OWNED, ServeError, ServeOptions};
use csub::service::ServiceOptions;

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

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("csub: start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(cli.command))
}

async fn run(command: Command) -> ExitCode {
    let registry_command = match command {
        Command::Serve {
            pool,
            port,
            state_dir,
            anthropic_base_url,
        } => {
            return match csub::serve::serve(ServeOptions {
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
                ServiceAction::Install { pool, port } => csub::service::install(&ServiceOptions {
                    pool_id: pool.pool,
                    config_path: pool.config,
                    port,
                }),
                ServiceAction::Uninstall => csub::service::uninstall(),
                ServiceAction::Status => csub::service::status(),
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
    match csub::cli::run(&registry_command).await {
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
