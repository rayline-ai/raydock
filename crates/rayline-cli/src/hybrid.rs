//! Explicit ARC hybrid runtime, separate from auth-free `--local` static routing.
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use rayline_local_router::ModelCapabilities;
use serde_json::{Value, json};

#[derive(Parser)]
#[command(
    name = "rayline hybrid",
    about = "Run ARC hybrid routing with externally managed inference"
)]
struct Cli {
    /// Separate runtime instance directory (contains private gateway credentials).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Start the Go execution core and authenticated loopback gateway.
    Start {
        /// Override the execution core installed beside rayline.
        #[arg(long)]
        core: Option<PathBuf>,
        /// Optional qualified execution-core profile. Without it, Rayline Local
        /// builds the default profile from your signed-in environment and
        /// `local_model`. Either way it applies `hybrid.routing` overrides.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Signed-in environment whose router makes the decisions (default: prod).
        #[arg(long, default_value = "prod")]
        env: String,
        /// Local runtime context window in tokens, when it cannot be read from
        /// the runtime (llama.cpp reports it; Ollama and MLX do not).
        #[arg(long)]
        local_context_window: Option<u64>,
        /// Isolated Decisions cell tag (development only).
        #[arg(long)]
        cell: Option<String>,
        /// Loopback gateway port; 0 chooses a free port.
        #[arg(long, default_value_t = 20815)]
        port: u16,
        /// Core initialization/readiness deadline, including runtime profile checks.
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
        startup_timeout_secs: u64,
    },
    /// Show live gateway/core readiness and endpoint (never prints keys).
    Status,
    /// Stop gateway and execution core; external model servers remain running.
    Stop,
    /// Connect a coding harness to the running hybrid gateway.
    Run {
        /// claude, codex, pi, opencode, omp, hermes or openclaw.
        harness: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

struct Paths {
    dir: PathBuf,
}
impl Paths {
    fn token(&self) -> PathBuf {
        self.dir.join("gateway-token")
    }
    fn ready(&self) -> PathBuf {
        self.dir.join("ready.json")
    }
    fn log(&self) -> PathBuf {
        self.dir.join("runtime.log")
    }
    fn supervisor(&self) -> PathBuf {
        self.dir.join("supervisor.json")
    }
    fn core_config(&self) -> PathBuf {
        self.dir.join("core-config.json")
    }
}

pub async fn run(args: Vec<OsString>) -> ExitCode {
    let cli =
        match Cli::try_parse_from(std::iter::once(OsString::from("rayline hybrid")).chain(args)) {
            Ok(cli) => cli,
            Err(error) => {
                let code = error.exit_code();
                let _ = error.print();
                return ExitCode::from(code as u8);
            }
        };
    match execute(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> io::Result<ExitCode> {
    if !cfg!(unix) {
        return Err(io::Error::other(
            "hybrid routing currently requires macOS or Linux; Windows lifecycle is not yet supported",
        ));
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "cannot determine macOS version for hybrid routing",
            ));
        }
        let version = String::from_utf8(output.stdout).map_err(io::Error::other)?;
        let major = version
            .trim()
            .split('.')
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| io::Error::other("invalid macOS version for hybrid routing"))?;
        if major < 13 {
            return Err(io::Error::other(
                "hybrid routing requires macOS 13 Ventura or later",
            ));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| io::Error::other("cannot find home directory"))?;
    let paths = Paths {
        dir: cli
            .state_dir
            .unwrap_or_else(|| home.join(".rayline/hybrid")),
    };
    private_dir(&paths.dir)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    match cli.command {
        Action::Start {
            core,
            config,
            env,
            local_context_window,
            cell,
            port,
            startup_timeout_secs,
        } => {
            let _lock = crate::router::acquire_router_lock(&paths.dir.join("runtime.lock"))?;
            clear_stopped_state(&paths)?;
            let core = match core {
                Some(path) => fs::canonicalize(path)?,
                None => installed_binary("rayline-core")?,
            };
            // A missing settings file is fine; an unreadable or malformed one
            // must not silently drop `local_model` or `hybrid.routing`.
            let (settings_path, settings) = match crate::status::read_settings_checked(&home)? {
                Some((path, value)) => (Some(path), Some(value)),
                None => (None, None),
            };
            let settings_error = |error| settings_error(settings_path.as_deref(), error);
            let origin = if config.is_some() {
                crate::hybrid_profile::ProfileOrigin::File
            } else {
                crate::hybrid_profile::ProfileOrigin::Default
            };
            let (profile, base_dir) = match config {
                Some(config) => {
                    if env != "prod" || cell.is_some() || local_context_window.is_some() {
                        eprintln!(
                            "Note: --env, --cell and --local-context-window apply to the default profile; the profile file at --config sets these itself"
                        );
                    }
                    let profile_path = fs::canonicalize(config)?;
                    let profile: Value = serde_json::from_slice(&fs::read(&profile_path)?)
                        .map_err(|error| {
                            io::Error::other(format!("invalid hybrid configuration: {error}"))
                        })?;
                    (
                        profile,
                        profile_path.parent().unwrap_or(Path::new("/")).to_owned(),
                    )
                }
                None => {
                    let hosted = crate::status::resolve_hosted_environment(&env, Some(&home))
                        .map_err(|error| {
                            io::Error::other(format!("unknown environment {env:?}: {error:?}"))
                        })?;
                    let (base_url, model) =
                        crate::hybrid_profile::custom_local_model(settings.as_ref()).ok_or_else(
                            || io::Error::other(
                                "hybrid routing needs your local model: run `rayline local custom --url <URL> --model <NAME>` first",
                            ),
                        )?;
                    let window = match local_context_window {
                        Some(window) => window,
                        None => crate::hybrid_profile::probe_llamacpp_context(&client, &base_url)
                            .await
                            .ok_or_else(|| io::Error::other(
                                "cannot read the local runtime's context window; pass --local-context-window <tokens>",
                            ))?,
                    };
                    if env == crate::status::PROD_ENV {
                        eprintln!(
                            "Note: hybrid routing v2 is in preview on the development router; the production router accepts it after the catalog migration. Use --env dev for the preview."
                        );
                    }
                    // `rayline auth login` is enough: mint the router key on first use.
                    if std::env::var_os("RAYLINE_ROUTER_API_KEY")
                        .is_none_or(|value| value.is_empty())
                    {
                        crate::claude::ensure_router_key(&env, &home, None, false)
                            .await
                            .map_err(|error| io::Error::other(error.to_string()))?;
                    }
                    let profile = crate::hybrid_profile::default_profile(
                        &hosted.router_url,
                        &base_url,
                        &model,
                        window,
                        cell.as_deref(),
                        settings.as_ref(),
                    )
                    .map_err(settings_error)?;
                    println!(
                        "Default hybrid profile: decisions via {} (model rayline-router), local window {window} tokens",
                        hosted.router_url
                    );
                    (profile, paths.dir.clone())
                }
            };
            let composed =
                crate::hybrid_profile::compose(profile, settings.as_ref(), &base_dir, origin)
                    .map_err(settings_error)?;
            // Decisions carry execution context and the router key: only a
            // configured Rayline router or loopback may receive them.
            let router_urls = crate::hybrid_profile::configured_router_urls(&home);
            crate::hybrid_profile::check_decisions_url(&composed.config, &router_urls)
                .map_err(io::Error::other)?;
            let credentials =
                crate::hybrid_profile::saved_credentials(&composed.config, &home, |name| {
                    std::env::var(name).ok()
                })
                .map_err(io::Error::other)?;
            write_private_json(&paths.core_config(), &composed.config, true)?;
            let config = paths.core_config();
            match (&composed.policy, &composed.preset) {
                (crate::hybrid_profile::PolicySource::LegacyMode(mode), _) => {
                    println!("Routing policy: none (profile pins Decisions api_mode {mode})")
                }
                (source, Some(preset)) => println!(
                    "Routing policy: {preset} ({})",
                    match source {
                        crate::hybrid_profile::PolicySource::Default => "Rayline Local default",
                        crate::hybrid_profile::PolicySource::Settings => "settings hybrid.routing",
                        _ => "profile",
                    }
                ),
                _ => {}
            }
            if let Some((id, url, model)) = &composed.local_model {
                println!("Local model: {model} at {url} (deployment {id}, from local_model)");
            }
            for (name, _) in &credentials {
                println!("Using the saved Rayline key for {name}");
            }
            let rld = if std::env::var_os("RLD_BIN").is_some() {
                crate::router::resolve_rld_bin(&home)?
            } else {
                installed_binary(crate::DAEMON_BIN)?
            };
            let log = private_log(&paths.log())?;
            let mut command = Command::new(rld);
            command
                .arg("hybrid")
                .arg("--core")
                .arg(core)
                .arg("--config")
                .arg(config)
                .arg("--port")
                .arg(port.to_string())
                .arg("--startup-timeout-secs")
                .arg(startup_timeout_secs.to_string())
                .arg("--token-file")
                .arg(paths.token())
                .arg("--ready-file")
                .arg(paths.ready())
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
            for router_url in &router_urls {
                command.arg("--router-url").arg(router_url);
            }
            for (name, key) in &credentials {
                command.env(name, key);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                // SAFETY: setsid is async-signal-safe and touches no Rust state.
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            let mut child = command.spawn()?;
            let startup = async {
                write_private_json(&paths.supervisor(), &json!({"pid":child.id()}), false)?;
                let started = Instant::now();
                loop {
                    if let Some(status) = child.try_wait()? {
                        return Err(io::Error::other(format!(
                            "hybrid runtime exited ({status}); see {}",
                            paths.log().display()
                        )));
                    }
                    if paths.ready().exists() {
                        let state = ready(&paths)?;
                        if state.get("pid").and_then(Value::as_u64) != Some(u64::from(child.id())) {
                            return Err(io::Error::other(
                                "hybrid startup process identity mismatch",
                            ));
                        }
                        health(&client, &paths, &state).await?;
                        println!(
                            "Hybrid routing ready at {}\nGateway credential: {}\nLogs: {}",
                            url(&state)?,
                            paths.token().display(),
                            paths.log().display()
                        );
                        return Ok(());
                    }
                    if started.elapsed() > Duration::from_secs(startup_timeout_secs + 5) {
                        return Err(io::Error::other(format!(
                            "hybrid startup timed out; see {}",
                            paths.log().display()
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            .await;
            if let Err(error) = startup {
                stop_spawned(&mut child).await?;
                clear_stopped_state(&paths)?;
                return Err(error);
            }
        }
        Action::Status => {
            let state = ready(&paths)?;
            let health = health(&client, &paths, &state).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"listen_url": url(&state)?, "health": health})
                )
                .map_err(io::Error::other)?
            );
        }
        Action::Stop => {
            let _lock = crate::router::acquire_router_lock(&paths.dir.join("runtime.lock"))?;
            if stopped(&paths)? {
                clear_stopped_state(&paths)?;
                println!("Hybrid runtime is stopped; stale state removed.");
                return Ok(ExitCode::SUCCESS);
            }
            let state = ready(&paths)?;
            // Shutdown remains available when core health fails. The private
            // gateway token authenticates this supervisor independently.
            let shutdown: Value = client
                .post(format!("{}/_rayline/shutdown", url(&state)?))
                .bearer_auth(fs::read_to_string(paths.token())?)
                .json(&json!({}))
                .send()
                .await
                .map_err(io::Error::other)?
                .error_for_status()
                .map_err(io::Error::other)?
                .json()
                .await
                .map_err(io::Error::other)?;
            if shutdown.get("pid") != state.get("pid")
                || shutdown.get("instance_id") != state.get("instance_id")
                || shutdown.get("stopping") != Some(&json!(true))
            {
                return Err(io::Error::other("hybrid shutdown identity mismatch"));
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while paths.ready().exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::other(
                        "hybrid shutdown timed out; inspect runtime.log",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            fs::remove_file(paths.supervisor())?;
            println!("Hybrid gateway and execution core stopped.");
        }
        Action::Run { harness, args } => {
            let state = ready(&paths)?;
            health(&client, &paths, &state).await?;
            let token = fs::read_to_string(paths.token())?;
            let mut command = harness_command(
                &paths,
                &harness,
                &args,
                url(&state)?,
                &token,
                &harness_capabilities(&paths, model_capabilities(&state)?)?,
            )?;
            for name in credential_env_names(&state)? {
                // A harness may use the same variable name for its local gateway
                // token. Preserve that explicit replacement, never the inherited key.
                let replaced_with_gateway = command
                    .get_envs()
                    .any(|(key, value)| key == name && value == Some(std::ffi::OsStr::new(&token)));
                if !replaced_with_gateway {
                    command.env_remove(name);
                }
            }
            let status = command.status()?;
            return Ok(ExitCode::from(status.code().unwrap_or(1) as u8));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn installed_binary(name: &str) -> io::Result<PathBuf> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("rayline executable has no parent directory"))?;
    fs::canonicalize(directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .map_err(|error| io::Error::new(error.kind(), format!("installed {name} is unavailable; run `rayline update` or reinstall the complete Rayline release: {error}")))
}

fn ready(paths: &Paths) -> io::Result<Value> {
    let state: Value =
        serde_json::from_slice(&fs::read(paths.ready())?).map_err(io::Error::other)?;
    if state.get("contract_version") != Some(&json!(1)) || state.get("ready") != Some(&json!(true))
    {
        return Err(io::Error::other("incompatible hybrid runtime metadata"));
    }
    credential_env_names(&state)?;
    model_capabilities(&state)?;
    if state
        .get("instance_id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(io::Error::other("missing execution core instance identity"));
    }
    let endpoint = reqwest::Url::parse(url(&state)?).map_err(io::Error::other)?;
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.port().is_none()
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return Err(io::Error::other("invalid hybrid loopback endpoint"));
    }
    Ok(state)
}

fn credential_env_names(state: &Value) -> io::Result<Vec<&str>> {
    let values = state
        .get("credential_env")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("missing hybrid credential environment references"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|name| !name.is_empty() && !name.contains(['=', '\0']))
                .ok_or_else(|| io::Error::other("invalid hybrid credential environment reference"))
        })
        .collect()
}

fn model_capabilities(state: &Value) -> io::Result<ModelCapabilities> {
    let capabilities: ModelCapabilities = serde_json::from_value(
        state
            .get("capabilities")
            .ok_or_else(|| io::Error::other("missing hybrid model capabilities"))?
            .clone(),
    )
    .map_err(io::Error::other)?;
    capabilities.validate().map_err(io::Error::other)?;
    Ok(capabilities)
}

/// The core's capabilities with the context window a harness should budget
/// against ([`crate::hybrid_profile::reachable_context_window`]), read from
/// the config this instance was started with. `start` refuses to rewrite it
/// while an instance runs. Output and modality limits stay the core's.
fn harness_capabilities(
    paths: &Paths,
    mut capabilities: ModelCapabilities,
) -> io::Result<ModelCapabilities> {
    let config: Value = match fs::read(paths.core_config()) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(capabilities),
        Err(error) => return Err(error),
    };
    capabilities.context_window =
        crate::hybrid_profile::reachable_context_window(&config, capabilities.context_window);
    Ok(capabilities)
}

fn url(state: &Value) -> io::Result<&str> {
    state
        .get("listen_url")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("missing gateway URL"))
}

async fn health(client: &reqwest::Client, paths: &Paths, state: &Value) -> io::Result<Value> {
    let health: Value = client
        .get(format!("{}/healthz", url(state)?))
        .bearer_auth(fs::read_to_string(paths.token())?)
        .send()
        .await
        .map_err(io::Error::other)?
        .error_for_status()
        .map_err(io::Error::other)?
        .json()
        .await
        .map_err(io::Error::other)?;
    if health.get("pid") != state.get("pid")
        || health.get("core_pid") != state.get("core_pid")
        || health.get("instance_id") != state.get("instance_id")
        || health.get("ready") != Some(&json!(true))
        || health.get("contract_version") != Some(&json!(1))
        || model_capabilities(&health)? != model_capabilities(state)?
    {
        return Err(io::Error::other("hybrid health/process identity mismatch"));
    }
    Ok(health)
}

fn harness_command(
    paths: &Paths,
    name: &str,
    args: &[OsString],
    endpoint: &str,
    token: &str,
    capabilities: &ModelCapabilities,
) -> io::Result<Command> {
    require_fresh_session(name, args)?;
    let mut command = Command::new(name);
    crate::codex::withhold_router_key(&mut command);
    command.env_remove("RAYLINE_EXEC_AUTH_TOKEN");
    match name {
        "claude" => {
            command
                .env("ANTHROPIC_BASE_URL", endpoint)
                .env("ANTHROPIC_AUTH_TOKEN", token)
                .env_remove("ANTHROPIC_API_KEY")
                .args(["--model", "rayline-router"]);
            let recovery = crate::claude::refusal_recovery_values(
                crate::claude::RoutingMode::Override,
                |name| std::env::var_os(name),
            );
            crate::claude::apply_refusal_recovery_env(&mut command, &recovery);
            // Inherited cloud/vendor modes must not bypass the explicit gateway.
            for name in [
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY",
            ] {
                command.env_remove(name);
            }
        }
        "codex" => {
            command.env("RAYLINE_HYBRID_GATEWAY_TOKEN", token);
            for setting in [
                "model_provider=\"rayline-hybrid\"".to_owned(),
                "model=\"rayline-router\"".to_owned(),
                "model_providers.rayline-hybrid.name=\"Rayline Hybrid\"".to_owned(),
                format!("model_providers.rayline-hybrid.base_url=\"{endpoint}/v1\""),
                "model_providers.rayline-hybrid.wire_api=\"responses\"".to_owned(),
                "model_providers.rayline-hybrid.env_key=\"RAYLINE_HYBRID_GATEWAY_TOKEN\""
                    .to_owned(),
            ] {
                command.arg("-c").arg(setting);
            }
        }
        _ => {
            let harness = crate::harness::Harness::from_command(name)
                .ok_or_else(|| io::Error::other(format!("unsupported hybrid harness {name:?}")))?;
            let args = harness.hybrid_args(args)?;
            let config_dir = paths.dir.join("harness").join(name);
            private_dir(&config_dir)?;
            let ctx = crate::harness::RenderContext {
                env_name: "hybrid",
                key: token,
                router_url: endpoint,
                config_dir: &config_dir,
                conversation_id: crate::harness::ConversationIdFallback {
                    forced: None,
                    env_needs_it: false,
                },
                allow_empty_signature: true,
            };
            let plan = harness
                .hybrid_plan(&ctx, capabilities)
                .map_err(io::Error::other)?;
            crate::harness::write_files(&config_dir, &plan.files)?;
            // Used only by harnesses which cannot transmit native session identity.
            let conversation_id = format!("hybrid-{:032x}", rand::random::<u128>());
            command = crate::harness::build_command(
                Path::new(name),
                &plan,
                Some(&conversation_id),
                &args,
            );
            command.env_remove("RAYLINE_EXEC_AUTH_TOKEN");
            return Ok(command);
        }
    }
    command.args(args);
    Ok(command)
}

fn require_fresh_session(harness: &str, args: &[OsString]) -> io::Result<()> {
    let codex_resume = harness == "codex"
        && (args
            .first()
            .is_some_and(|arg| arg == "resume" || arg == "fork")
            || (args.first().is_some_and(|arg| arg == "exec")
                && args.get(1).is_some_and(|arg| arg == "resume")));
    let resuming = codex_resume
        || args.iter().take_while(|arg| *arg != "--").any(|arg| {
            let text = arg.to_string_lossy();
            let flag = text.split('=').next().unwrap_or("");
            match harness {
                "claude" => matches!(
                    flag,
                    "--resume"
                        | "--continue"
                        | "--fork-session"
                        | "--resume-session-at"
                        | "-r"
                        | "-c"
                ),
                "pi" => matches!(
                    flag,
                    "--resume"
                        | "--continue"
                        | "--fork"
                        | "--session"
                        | "--session-id"
                        | "-r"
                        | "-c"
                ),
                _ => false,
            }
        });
    if resuming {
        return Err(io::Error::other(
            "hybrid execution requires a fresh harness session; saved sessions do not contain the core's private-append ledger",
        ));
    }
    Ok(())
}

fn stopped(paths: &Paths) -> io::Result<bool> {
    if !paths.supervisor().exists() {
        if paths.ready().exists() || paths.token().exists() {
            return Err(io::Error::other(
                "hybrid runtime state has no supervisor identity; inspect the state directory before removing it",
            ));
        }
        return Ok(true);
    }
    let state: Value =
        serde_json::from_slice(&fs::read(paths.supervisor())?).map_err(io::Error::other)?;
    let pid = state
        .get("pid")
        .and_then(Value::as_i64)
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("invalid hybrid supervisor identity"))?;
    Ok(!crate::router::process_exists(pid))
}

fn clear_stopped_state(paths: &Paths) -> io::Result<()> {
    if !stopped(paths)? {
        return Err(io::Error::other(
            "hybrid supervisor is running; use `rayline hybrid status` or `rayline hybrid stop` first",
        ));
    }
    for path in [paths.ready(), paths.token(), paths.supervisor()] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn stop_spawned(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let pid = i32::try_from(child.id()).map_err(io::Error::other)?;
    match crate::router::terminate_process(pid) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            crate::router::force_stop_process_group(pid)?;
            child.wait()?;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A composition error, naming the settings file when the settings caused it.
fn settings_error(settings_path: Option<&Path>, error: String) -> io::Error {
    match settings_path {
        Some(path) if error.starts_with("settings ") => {
            io::Error::other(format!("{}: {error}", path.display()))
        }
        _ => io::Error::other(error),
    }
}

/// Replace `path` atomically with `value` as a 0600 file: a planted symlink
/// or a loosened file at `path` is replaced, never followed or kept.
fn write_private_json(path: &Path, value: &Value, pretty: bool) -> io::Result<()> {
    let contents = if pretty {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    }
    .map_err(io::Error::other)?;
    crate::claude_app::write_private(path, &contents)
}

/// Open the append-only runtime log owner-only: never through a symlink, and
/// narrowed to 0600 even when an older file was looser.
fn private_log(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "rayline-hybrid-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn state_files_replace_a_planted_symlink_without_writing_through_it() {
        let dir = temp_dir("symlink");
        let state = dir.join("state");
        private_dir(&state).unwrap();
        let victim = dir.join("victim.json");
        fs::write(&victim, "keep").unwrap();
        for target in ["core-config.json", "supervisor.json"] {
            let path = state.join(target);
            std::os::unix::fs::symlink(&victim, &path).unwrap();
            write_private_json(&path, &json!({"ok": true}), true).unwrap();
            assert!(
                !fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(written, json!({"ok": true}));
        }
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        // The append-only log refuses a planted symlink outright.
        std::os::unix::fs::symlink(&victim, state.join("runtime.log")).unwrap();
        assert!(private_log(&state.join("runtime.log")).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn state_files_end_up_owner_only_whatever_was_there() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("core-config.json");
        fs::write(
            &path,
            "{\"stale\": true, \"padding\": \"longer than the new file\"}",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private_json(&path, &json!({"fresh": 1}), false).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"fresh\":1}");
        let log = dir.join("runtime.log");
        fs::write(&log, "old\n").unwrap();
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        drop(private_log(&log).unwrap());
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&log).unwrap(), "old\n");
        // No temporary file is left behind.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_errors_name_the_settings_file() {
        let path = Path::new("/home/user/.config/rayline/settings.json");
        let error = settings_error(
            Some(path),
            "settings hybrid.routing.value_usd must be a number between 0.1 and 5".to_owned(),
        );
        assert_eq!(
            error.to_string(),
            "/home/user/.config/rayline/settings.json: settings hybrid.routing.value_usd must be a number between 0.1 and 5"
        );
        let other = settings_error(
            Some(path),
            "local deployment protocol is missing".to_owned(),
        );
        assert_eq!(other.to_string(), "local deployment protocol is missing");
    }
    #[test]
    fn hybrid_is_explicit_and_requires_core_and_config() {
        // A profile file is optional: the default profile needs only a key and local_model.
        assert!(Cli::try_parse_from(["hybrid", "start"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "hybrid",
                "start",
                "--env",
                "dev",
                "--local-context-window",
                "16384"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["hybrid", "start", "--core", "/core", "--config", "/config"])
                .is_ok()
        );
        for option in ["--env=dev", "--auth-token=synthetic"] {
            let argv: Vec<OsString> = ["rayline", option, "hybrid", "status"]
                .into_iter()
                .map(OsString::from)
                .collect();
            assert_eq!(
                crate::rayline_dispatch_for_argv(&argv),
                crate::RaylineDispatch::Unavailable
            );
        }
        assert!(Cli::try_parse_from(["hybrid", "run", "claude", "--", "-p", "hello"]).is_ok());
    }
    #[test]
    fn cloud_credentials_are_not_exported_to_harnesses() {
        let paths = Paths {
            dir: PathBuf::from("/unused"),
        };
        for name in ["claude", "codex"] {
            let command = harness_command(
                &paths,
                name,
                &[],
                "http://127.0.0.1:20815",
                "local-token",
                &ModelCapabilities {
                    context_window: 16384,
                    max_output_tokens: 8192,
                    input_modalities: vec!["text".into()],
                },
            )
            .unwrap();
            let env: std::collections::HashMap<_, _> = command.get_envs().collect();
            assert_eq!(
                env.get(std::ffi::OsStr::new("RAYLINE_ROUTER_API_KEY")),
                Some(&None)
            );
            assert_eq!(
                env.get(std::ffi::OsStr::new("RAYLINE_EXEC_AUTH_TOKEN")),
                Some(&None)
            );
        }
    }

    fn pi_models_for(profile: &Value) -> Value {
        let paths = Paths {
            dir: temp_dir("pi-window"),
        };
        write_private_json(&paths.core_config(), profile, true).unwrap();
        let core = ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 4096,
            input_modalities: vec!["text".into()],
        };
        let capabilities = harness_capabilities(&paths, core).unwrap();
        harness_command(
            &paths,
            "pi",
            &[],
            "http://127.0.0.1:20815",
            "local-token",
            &capabilities,
        )
        .unwrap();
        let models: Value =
            serde_json::from_slice(&fs::read(paths.dir.join("harness/pi/models.json")).unwrap())
                .unwrap();
        let _ = fs::remove_dir_all(&paths.dir);
        models["providers"]["rayline"]["models"][0].clone()
    }

    #[test]
    fn pi_gets_the_cloud_window_when_handoff_can_reach_cloud() {
        let profile = crate::hybrid_profile::default_profile(
            "https://api-dev.rayline.ai",
            "http://127.0.0.1:11434/v1",
            "qwen3.8:27b",
            16384,
            None,
            None,
        )
        .unwrap();
        let model = pi_models_for(&profile);
        assert_eq!(model["contextWindow"], 200_000);
        // Output and modality limits stay the core's common envelope.
        assert_eq!(model["maxTokens"], 4096);
        assert_eq!(model["input"], json!(["text"]));
    }

    #[test]
    fn pi_keeps_the_local_window_without_a_cloud_deployment() {
        let profile = json!({"deployments": [
            {"id": "local", "kind": "local", "context_window": 16384, "max_output_tokens": 4096}
        ]});
        assert_eq!(pi_models_for(&profile)["contextWindow"], 16384);
    }

    #[test]
    fn historical_sessions_require_an_explicit_restore_contract() {
        for (harness, arg) in [
            ("claude", "--resume=saved"),
            ("claude", "--continue"),
            ("claude", "-r"),
            ("codex", "resume"),
            ("codex", "fork"),
            ("pi", "--fork"),
            ("pi", "--session=saved"),
        ] {
            assert!(require_fresh_session(harness, &[OsString::from(arg)]).is_err());
        }
        assert!(
            require_fresh_session(
                "claude",
                &[
                    OsString::from("-p"),
                    OsString::from("Explain resume support")
                ]
            )
            .is_ok()
        );
    }
}
