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

/// How long `start` waits for the local model's tokenizer files before it
/// uses a cached copy or starts without a prompt counter.
const TOKENIZER_FETCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Parser)]
#[command(
    name = "raydock hybrid",
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
        /// Optional qualified execution-core profile. Without it, Raydock
        /// builds the default profile from your signed-in environment and
        /// `local_model`. Either way it applies `hybrid.routing` overrides.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Signed-in environment whose router makes the decisions (default: prod).
        #[arg(long, default_value = "prod")]
        env: String,
        /// Local model context window in tokens (default: settings
        /// `hybrid.local_context_window`, else the window the runtime reports
        /// through llama.cpp `/props` or `/v1/models` from vLLM or TensorFold,
        /// else 98304). A turn whose prompt does not fit hands off to cloud.
        /// The runtime's loaded window caps it.
        #[arg(long, value_parser = clap::value_parser!(u64).range(2048..=1_000_000))]
        local_context_window: Option<u64>,
        /// Model for Claude Code's auto-mode safety classifier, sent through
        /// your cloud router (default: settings `hybrid.classifier_model`, else
        /// claude-haiku-5-5).
        #[arg(long)]
        classifier_model: Option<String>,
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
        match Cli::try_parse_from(std::iter::once(OsString::from("raydock hybrid")).chain(args)) {
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
            .unwrap_or_else(|| home.join(".rayline").join("hybrid")),
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
            classifier_model,
            cell,
            port,
            startup_timeout_secs,
        } => {
            let _lock = acquire_lock(&paths.dir.join("runtime.lock"))?;
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
            let classifier_model = crate::hybrid_profile::classifier_model(
                classifier_model.as_deref(),
                settings.as_ref(),
            )
            .map_err(settings_error)?;
            let origin = if config.is_some() {
                crate::hybrid_profile::ProfileOrigin::File
            } else {
                crate::hybrid_profile::ProfileOrigin::Default
            };
            // A local model other than the default Qwen3.8-27B, declared in
            // settings: named in the error if the core refuses to start.
            let mut declared_local = None;
            let mut token_counter_dir = None;
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
                    let hosted = crate::status::resolve_hosted_environment(
                        (env != crate::status::PROD_ENV).then_some(env.as_str()),
                        &home,
                    )
                    .map_err(|error| {
                        io::Error::other(format!("unknown environment {env:?}: {error}"))
                    })?;
                    let (base_url, model) =
                        crate::hybrid_profile::custom_local_model(settings.as_ref()).ok_or_else(
                            || io::Error::other(
                                "hybrid routing needs your local model: set local_model.base_url and local_model.model in settings.json",
                            ),
                        )?;
                    let configured = match local_context_window {
                        Some(window) => Some(window),
                        None => crate::hybrid_profile::configured_local_context_window(
                            settings.as_ref(),
                        )
                        .map_err(settings_error)?,
                    };
                    let registry_model =
                        crate::hybrid_profile::local_registry_model(settings.as_ref())
                            .map_err(settings_error)?;
                    let runtime =
                        crate::hybrid_profile::read_local_runtime(&client, &base_url, &model).await;
                    // The identity check knows only Qwen3.8-27B; a declared
                    // model is the user's statement and is not checked.
                    let (local_refused, message) = crate::hybrid_profile::local_verdict(
                        &registry_model,
                        &runtime.identity,
                        &base_url,
                    );
                    if let Some(message) = message {
                        eprintln!("{message}");
                    }
                    if registry_model != crate::hybrid_profile::LOCAL_REGISTRY_MODEL {
                        declared_local = Some(registry_model.clone());
                    }
                    if !local_refused
                        && let Some(repo) = crate::hybrid_tokenizer::tokenizer_repo(
                            &registry_model,
                            settings.as_ref(),
                        )
                        .map_err(settings_error)?
                    {
                        // The Hub gets a short deadline; past it the cached
                        // snapshot, if any, is used and startup goes on.
                        // A detached thread, not a blocking task: the runtime
                        // waits for blocking tasks at exit, and the downloader
                        // retries for far longer than `start` should run.
                        let (sender, fetching) = tokio::sync::oneshot::channel();
                        std::thread::spawn({
                            let repo = repo.clone();
                            move || {
                                let _ = sender.send(crate::hybrid_tokenizer::fetch(&repo));
                            }
                        });
                        let fetched = match tokio::time::timeout(TOKENIZER_FETCH_DEADLINE, fetching)
                            .await
                        {
                            Ok(Ok(fetched)) => fetched,
                            Ok(Err(_)) => Err("the tokenizer download stopped".to_owned()),
                            Err(_) => crate::hybrid_tokenizer::cached(&repo).ok_or_else(|| {
                                format!(
                                    "Hugging Face did not answer within {}s and nothing is cached",
                                    TOKENIZER_FETCH_DEADLINE.as_secs()
                                )
                            }),
                        };
                        match fetched {
                            Ok(dir) => {
                                println!(
                                    "Prompt counter: {repo} (the local model's tokenizer and chat template)"
                                );
                                token_counter_dir = Some(dir);
                            }
                            Err(error) => eprintln!(
                                "Prompt counter unavailable ({error}); the core estimates the local model's prompt size, which can send turns to the cloud early"
                            ),
                        }
                    }
                    let window = if local_refused {
                        configured.unwrap_or(crate::hybrid_profile::DEFAULT_LOCAL_CONTEXT_WINDOW)
                    } else {
                        let (window, note) =
                            crate::hybrid_profile::resolve_local_window(configured, runtime.window);
                        if let Some(note) = note {
                            eprintln!("{note}");
                        }
                        if let Some(warning) =
                            crate::hybrid_profile::small_window_warning(&registry_model, window)
                        {
                            eprintln!("{warning}");
                        }
                        window
                    };
                    // `raydock auth login` is enough: mint the router key on first use.
                    if std::env::var_os("RAYLINE_ROUTER_API_KEY")
                        .is_none_or(|value| value.is_empty())
                    {
                        crate::status::router_key(&hosted, &home)
                            .await
                            .map_err(|error| io::Error::other(error.to_string()))?;
                    }
                    let mut profile = crate::hybrid_profile::default_profile(
                        &hosted.base_url,
                        &base_url,
                        &model,
                        window,
                        cell.as_deref(),
                        settings.as_ref(),
                    )
                    .map_err(settings_error)?;
                    if !local_refused && runtime.refuses_over_window {
                        crate::hybrid_profile::mark_local_refuses_over_window(&mut profile);
                    }
                    if local_refused {
                        crate::hybrid_profile::without_local(&mut profile);
                        println!(
                            "Default hybrid profile: decisions via {} (model {}), cloud only",
                            hosted.base_url,
                            crate::hybrid_profile::DECISIONS_MODEL
                        );
                    } else {
                        println!(
                            "Default hybrid profile: decisions via {} (model {}), local window {window} tokens",
                            hosted.base_url,
                            crate::hybrid_profile::DECISIONS_MODEL
                        );
                    }
                    (profile, paths.dir.clone())
                }
            };
            let composed =
                crate::hybrid_profile::compose(profile, settings.as_ref(), &base_dir, origin)
                    .map_err(settings_error)?;
            // Decisions carry execution context and the router key: only a
            // configured Rayline router or loopback may receive them.
            let router_urls =
                crate::hybrid_profile::configured_router_urls(&home).map_err(io::Error::other)?;
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
                        crate::hybrid_profile::PolicySource::Default => "Raydock default",
                        crate::hybrid_profile::PolicySource::Settings => "settings hybrid.routing",
                        _ => "profile",
                    }
                ),
                _ => {}
            }
            if let Some((id, url, model)) = &composed.local_model {
                println!("Local model: {model} at {url} (deployment {id}, from local_model)");
            }
            println!(
                "Auto-mode classifier: {classifier_model} (Claude Code's safety checks; change with settings hybrid.classifier_model or --classifier-model)"
            );
            for (name, _) in &credentials {
                println!("Using the saved Rayline key for {name}");
            }
            // One Decisions call before reporting ready: a refused key or an
            // unreachable router fails here, not on the harness's first turn.
            // The environment whose router serves Decisions: a profile file may
            // name another one than `--env`. `None` is the built-in prod, whose
            // commands take no `--env`.
            let decisions_env = match composed.config.pointer("/decisions/url") {
                Some(Value::String(url)) => crate::hybrid_profile::environment_for_url(&home, url)
                    .map_err(io::Error::other)?
                    .map(|hosted| hosted.env),
                _ => None,
            }
            .unwrap_or_else(|| (env != crate::status::PROD_ENV).then(|| env.clone()));
            probe_decisions(
                &composed.config,
                &credentials,
                decisions_env.as_deref(),
                startup_timeout_secs,
            )
            .await?;
            let rld = installed_binary(crate::DAEMON_BIN)?;
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
            if let Some(env_name) = &decisions_env {
                command.arg("--env-name").arg(env_name);
            }
            command.arg("--classifier-model").arg(&classifier_model);
            if let Some(dir) = &token_counter_dir {
                command.arg("--token-counter-dir").arg(dir);
            }
            for (name, key) in &credentials {
                command.env(name, key);
            }
            let mut child = crate::detach::spawn_detached(&mut command)?;
            let startup = async {
                write_private_json(&paths.supervisor(), &json!({"pid":child.id()}), false)?;
                let started = Instant::now();
                loop {
                    if let Some(status) = child.try_wait()? {
                        return Err(io::Error::other(exit_message(
                            &status.to_string(),
                            &paths.log(),
                            declared_local.as_deref(),
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
            let _lock = acquire_lock(&paths.dir.join("runtime.lock"))?;
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
                &harness_capabilities(model_capabilities(&state)?),
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
            // Here, not in harness_command: preparing may run Hermes, and the
            // probe and repair must carry the launch's final environment.
            if harness == "hermes" && cfg!(windows) {
                crate::harness::hermes::prepare_launch(&command).map_err(io::Error::other)?;
            }
            let status = command.status()?;
            return Ok(ExitCode::from(status.code().unwrap_or(1) as u8));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Send one Decisions request with the key the core will use and classify
/// the answer. A development cell (`cell_tag`) gets a real routing request
/// (the profile's model, and its V when it has one), which also starts a scale-to-zero cell; while it answers 502/503/504 the
/// probe waits and retries within the startup deadline. Without a cell an
/// empty body is enough: the router authenticates before it validates.
/// Skipped for a loopback Decisions URL.
async fn probe_decisions(
    config: &Value,
    credentials: &[(String, String)],
    env: Option<&str>,
    startup_timeout_secs: u64,
) -> io::Result<()> {
    let env_flag = env.map(|env| format!("--env {env} ")).unwrap_or_default();
    let Some(decisions) = config.get("decisions") else {
        return Ok(());
    };
    let (Some(url), Some(auth_env)) = (
        decisions.get("url").and_then(Value::as_str),
        decisions.get("auth_env").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    // A loopback Decisions service is a local fixture or development
    // service, not a router this check can describe.
    if reqwest::Url::parse(url).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            rayline_local_router::decisions_url::is_loopback(&host.to_ascii_lowercase())
        })
    }) {
        return Ok(());
    }
    let saved = credentials
        .iter()
        .find(|(name, _)| name == auth_env)
        .map(|(_, key)| key.clone());
    let from_environment = saved.is_none();
    let key = saved
        .or_else(|| std::env::var(auth_env).ok().filter(|key| !key.is_empty()))
        .ok_or_else(|| {
            io::Error::other(format!(
                "no Rayline key for Decisions ({auth_env}); run `raydock {env_flag}auth login`"
            ))
        })?;
    let cell = decisions.get("cell_tag").and_then(Value::as_str);
    let body = probe_body(decisions);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let deadline = Instant::now() + Duration::from_secs(startup_timeout_secs);
    let mut announced = false;
    loop {
        let mut request = client.post(url).bearer_auth(&key).json(&body);
        if let Some(cell) = cell {
            request = request.header("X-Rayline-Cell", cell);
        }
        let outcome = match request.send().await {
            Ok(response) => Ok(response.status().as_u16()),
            Err(error) => Err(error),
        };
        match probe_verdict(outcome.as_ref().copied().map_err(|_| ())) {
            ProbeVerdict::Ready => return Ok(()),
            ProbeVerdict::KeyRefused(status) => {
                let fix = if from_environment {
                    format!("Check or unset {auth_env}, which overrides the saved key")
                } else {
                    format!("Run `raydock {env_flag}auth login`")
                };
                return Err(io::Error::other(format!(
                    "Decisions at {url} refused the Rayline key (HTTP {status}). {fix}, then `raydock hybrid start` again."
                )));
            }
            ProbeVerdict::RateLimited => {
                return Err(io::Error::other(format!(
                    "Decisions at {url} refused the readiness check with HTTP 429: the Rayline key is rate limited or out of quota; check your account, then retry"
                )));
            }
            ProbeVerdict::Rejected(status) => {
                return Err(io::Error::other(format!(
                    "Decisions at {url} answered HTTP {status} to a readiness check; hybrid routing cannot start"
                )));
            }
            ProbeVerdict::Unavailable => {
                if Instant::now() >= deadline {
                    let detail = match &outcome {
                        Ok(status) => format!("HTTP {status}"),
                        Err(error) => error.to_string(),
                    };
                    return Err(io::Error::other(format!(
                        "Decisions at {url} is unreachable or not ready ({detail}); check your network and `raydock {env_flag}status`, then retry"
                    )));
                }
                if !announced {
                    announced = true;
                    match cell {
                        Some(cell) => println!(
                            "Waiting for Decisions cell {cell} to start (it scales to zero)..."
                        ),
                        None => println!("Waiting for Decisions at {url}..."),
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ProbeVerdict {
    Ready,
    KeyRefused(u16),
    RateLimited,
    Unavailable,
    Rejected(u16),
}

/// `Err(())`: no HTTP answer (connect error or timeout).
/// The readiness body: empty without a cell, else a routing request with the
/// profile's model, and its V when it has one.
fn probe_body(decisions: &Value) -> Value {
    if decisions.get("cell_tag").and_then(Value::as_str).is_none() {
        return json!({});
    }
    let mut body = json!({
        "model": decisions.get("model"),
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "raydock hybrid start: readiness check"}]
    });
    if let Some(v_value) = decisions.pointer("/routing/v_value") {
        body["routing"] = json!({"v_value": v_value});
    }
    body
}

fn probe_verdict(outcome: Result<u16, ()>) -> ProbeVerdict {
    match outcome {
        Err(()) => ProbeVerdict::Unavailable,
        Ok(status) if (200..300).contains(&status) => ProbeVerdict::Ready,
        Ok(status @ (401 | 403)) => ProbeVerdict::KeyRefused(status),
        Ok(429) => ProbeVerdict::RateLimited,
        Ok(500 | 502 | 503 | 504) => ProbeVerdict::Unavailable,
        // Authenticated, then refused the readiness body (an empty one, or a
        // cell that wants an execution context): the key and router work.
        // A 404/405 means the URL is not a Decisions route: rejected.
        Ok(400 | 409 | 422) => ProbeVerdict::Ready,
        Ok(status) => ProbeVerdict::Rejected(status),
    }
}

fn installed_binary(name: &str) -> io::Result<PathBuf> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("rayline executable has no parent directory"))?;
    fs::canonicalize(directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .map_err(|error| io::Error::new(error.kind(), format!("installed {name} is unavailable; run `raydock update` or reinstall the complete Raydock release: {error}")))
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

/// Read-only connection for top: never creates state or starts a runtime.
/// The client's default credentials are safe only for this validated origin.
pub(crate) async fn top_connection(
    state_dir: &Path,
) -> io::Result<(reqwest::Client, String, Value)> {
    let paths = Paths {
        dir: state_dir.to_owned(),
    };
    let state = ready(&paths)
        .map_err(|_| io::Error::other("hybrid runtime readiness is unavailable or invalid"))?;
    for key in ["pid", "core_pid"] {
        state
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| process_exists(*pid))
            .ok_or_else(|| io::Error::other("hybrid runtime process is not live"))?;
    }
    let token = fs::read_to_string(paths.token())
        .map_err(|_| io::Error::other("hybrid gateway credential is unavailable"))?;
    if token.trim().is_empty() {
        return Err(io::Error::other("hybrid gateway credential is invalid"));
    }
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| io::Error::other("hybrid gateway credential is invalid"))?;
    authorization.set_sensitive(true);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, authorization);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .default_headers(headers)
        .build()
        .map_err(|_| io::Error::other("cannot initialize hybrid gateway connection"))?;
    health(&client, &paths, &state).await.map_err(|_| {
        io::Error::other("hybrid gateway health/authentication or process identity check failed")
    })?;
    let endpoint = url(&state)?.trim_end_matches('/');
    Ok((client, format!("{endpoint}/v1/router/top/snapshot"), state))
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

/// Claude Code's switch between the API's in-turn auto-mode checks and its
/// own classifier requests (code.claude.com auto-mode classifier docs).
const AUTO_MODE_SERVER_ENV: &str = "CLAUDE_CODE_AUTO_MODE_SERVER";

/// The value to set for [`AUTO_MODE_SERVER_ENV`]: `0` unless the user's
/// environment already names one (an empty value included), which is kept.
fn auto_mode_server_default(inherited: Option<&std::ffi::OsStr>) -> Option<&'static str> {
    inherited.is_none().then_some("0")
}
/// Context window every harness is told under hybrid routing. The runtime
/// carries any prompt up to this size: a turn too large for the local model
/// hands off to cloud, and a prompt larger than the cloud window is refused
/// ("prompt is too long") so the harness compacts.
pub(crate) const HARNESS_CONTEXT_WINDOW: u64 = 1_000_000;

/// The core's capabilities with [`HARNESS_CONTEXT_WINDOW`] as the window.
/// Output and modality limits stay the core's admitted ones.
fn harness_capabilities(mut capabilities: ModelCapabilities) -> ModelCapabilities {
    capabilities.context_window = HARNESS_CONTEXT_WINDOW;
    capabilities
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
    let program = crate::harness::launch_program(name, std::env::var_os("PATH"));
    let mut command = Command::new(&program);
    crate::codex::withhold_router_key(&mut command);
    command.env_remove("RAYLINE_EXEC_AUTH_TOKEN");
    match name {
        "claude" => {
            command
                .env("ANTHROPIC_BASE_URL", endpoint)
                .env("ANTHROPIC_AUTH_TOKEN", token)
                .env_remove("ANTHROPIC_API_KEY")
                // Claude Code otherwise asks for its own output default
                // (32k), which the core refuses.
                .env(
                    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
                    capabilities.max_output_tokens.to_string(),
                )
                // `rayline-auto` is not in Claude Code's model catalog, so it
                // assumes a 200k window and auto-compacts long before the
                // gateway's. State the window the gateway advertises, as
                // `model_context_window` already does for codex.
                .env(
                    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
                    capabilities.context_window.to_string(),
                )
                .args(["--model", "rayline-auto"]);
            // Claude Code 2.1.321+ asks the API to judge auto mode's actions
            // inside each turn (`safeguards`, answered in `safeguard_results`).
            // A turn the local model serves carries no verdict, so every
            // checked action is denied. Its own classifier requests, which the
            // gateway sends to the cloud classifier pin, judge them instead.
            // A value the user set is kept.
            if let Some(value) =
                auto_mode_server_default(std::env::var_os(AUTO_MODE_SERVER_ENV).as_deref())
            {
                command.env(AUTO_MODE_SERVER_ENV, value);
            }
            let recovery = crate::claude::refusal_recovery_values(|name| std::env::var_os(name));
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
                "model=\"rayline-auto\"".to_owned(),
                "model_providers.rayline-hybrid.name=\"Raydock Hybrid\"".to_owned(),
                format!("model_providers.rayline-hybrid.base_url=\"{endpoint}/v1\""),
                "model_providers.rayline-hybrid.wire_api=\"responses\"".to_owned(),
                "model_providers.rayline-hybrid.env_key=\"RAYLINE_HYBRID_GATEWAY_TOKEN\""
                    .to_owned(),
                format!("model_context_window={}", capabilities.context_window),
                format!("model_max_output_tokens={}", capabilities.max_output_tokens),
            ] {
                command.arg("-c").arg(setting);
            }
        }
        _ => {
            let harness = crate::harness::Harness::from_command(name)
                .ok_or_else(|| io::Error::other(format!("unsupported hybrid harness {name:?}")))?;
            let major_version =
                harness.major_version(crate::harness::find_on_path(harness.binary()).as_deref());
            let args = harness.hybrid_args(args, major_version)?;
            let config_dir = paths.dir.join("harness").join(name);
            private_dir(&config_dir)?;
            let ctx = crate::harness::RenderContext {
                key: token,
                router_url: endpoint,
                config_dir: &config_dir,
                conversation_id: crate::harness::ConversationIdFallback::new(
                    std::env::var(crate::harness::CONV_ID_FALLBACK_ENV)
                        .ok()
                        .as_deref(),
                ),
                allow_empty_signature: true,
                capabilities: Some(capabilities),
                harness_major_version: major_version,
                models: crate::harness::LaunchModels::hybrid(capabilities),
            };
            let plan = harness.plan(&ctx).map_err(io::Error::other)?;
            crate::harness::write_files(&config_dir, &plan.files)?;
            // The per-launch id is opt-in here as on the hosted path, and is
            // exported only when a rendered config references it.
            let conversation_id =
                crate::harness::conversation_id_for(&plan, crate::harness::new_conversation_id);
            command =
                crate::harness::build_command(&program, &plan, conversation_id.as_deref(), &args);
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
                "opencode" => matches!(flag, "--continue" | "--session" | "--fork" | "-c" | "-s"),
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
    Ok(!process_exists(pid))
}

fn clear_stopped_state(paths: &Paths) -> io::Result<()> {
    if !stopped(paths)? {
        return Err(io::Error::other(
            "hybrid supervisor is running; use `raydock hybrid status` or `raydock hybrid stop` first",
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
    match request_supervisor_stop(pid) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            force_stop_process_group(pid)?;
            child.wait()?;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Ask a supervisor we spawned to stop and clean up after itself. Unix sends
/// SIGTERM; Windows sets the stop event `rld` waits on. `NotFound` means it
/// is already gone.
fn request_supervisor_stop(pid: i32) -> io::Result<()> {
    #[cfg(unix)]
    {
        terminate_process(pid)
    }
    #[cfg(windows)]
    {
        let pid = u32::try_from(pid).map_err(io::Error::other)?;
        rayline_local_router::windows_state::request_stop(pid)
    }
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    // Files created inside inherit the owner-only entry from the start.
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, true)?;
    Ok(())
}

/// A composition error, naming the settings file when the settings caused it.
/// The error for a runtime that exits during startup. With a declared local
/// model it names the setting: a core whose embedded registry does not know
/// the model refuses the profile and exits.
fn exit_message(status: &str, log: &Path, declared_local: Option<&str>) -> String {
    let mut message = format!("hybrid runtime exited ({status}); see {}", log.display());
    if let Some(model) = declared_local {
        message.push_str(&format!(
            ". If the log says the core does not know {model}, it is not in this core's model registry: correct settings hybrid.local_registry_model, or remove it to use Qwen3.8-27B"
        ));
    }
    message
}

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
    crate::claude_app::write_private(path, &contents)?;
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, false)?;
    Ok(())
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
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        // Open a planted link itself, not its target, so it is refused below.
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, false)?;
    Ok(file)
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Mirrors POSIX kill(pid, 0): no signal is delivered; errno tells
    // us whether the process is absent or merely owned by another user.
    // SAFETY: kill with signal 0 only checks the pid; it touches no memory.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return !process_is_zombie(pid);
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn process_is_zombie(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|output| output.status.success().then_some(output.stdout))
        .map(|stdout| String::from_utf8_lossy(&stdout).contains('Z'))
        .unwrap_or(false)
}

#[cfg(unix)]
fn terminate_process(pid: i32) -> io::Result<()> {
    signal_process(pid, libc::SIGTERM)
}

#[cfg(unix)]
fn force_stop_process_group(pid: i32) -> io::Result<()> {
    // SAFETY: killpg takes plain integers and touches no memory.
    let result = unsafe { libc::killpg(pid, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

#[cfg(unix)]
fn signal_process(pid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: kill takes plain integers and touches no memory.
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Err(io::Error::new(io::ErrorKind::NotFound, error));
    }
    Err(error)
}

#[cfg(windows)]
fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let query = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}').ProcessId");
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &query])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == pid.to_string()
        })
}

#[cfg(windows)]
fn force_stop_process_group(pid: i32) -> io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "taskkill /T /F failed with status {status}"
        )))
    }
}

/// Acquire an exclusive advisory lock on `lock_path`, blocking until it is
/// available.  The lock is automatically released when the returned `File`
/// is dropped (i.e. when the guard goes out of scope or the process dies).
///
/// Unix uses `flock`; Windows uses `LockFileEx`, which blocks the same way and
/// is released when the handle closes.
#[cfg(unix)]
fn acquire_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    // SAFETY: a valid file descriptor owned by `file` for this call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(windows)]
fn acquire_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    // Lock the whole range; nothing reads or writes the lock file's bytes.
    // SAFETY: a valid file handle and a zeroed OVERLAPPED (offset 0), used
    // synchronously because the file was not opened for overlapped I/O.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if locked == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The built-in prod takes no `--env`: `--env prod` selects a configured
    // `environments.prod` instead.
    #[test]
    fn a_missing_decisions_key_names_the_login_for_its_environment() {
        let config = serde_json::json!({"decisions": {
            "url": "https://decisions.example.test/v1/decisions",
            "auth_env": "RAYLINE_TEST_UNSET_DECISIONS_KEY",
        }});
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (env, login) in [
            (None, "`raydock auth login`"),
            (Some("dev"), "`raydock --env dev auth login`"),
        ] {
            let error = runtime
                .block_on(probe_decisions(&config, &[], env, 1))
                .unwrap_err()
                .to_string();
            assert!(error.ends_with(login), "{error}");
        }
    }

    /// Serializes the tests that set a process-wide env var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[cfg(windows)]
    #[test]
    fn state_files_end_up_owner_only_on_windows() {
        use rayline_local_router::windows_state::is_owner_only;
        let dir = temp_dir("acl");
        let state = dir.join("state");
        private_dir(&state).unwrap();
        assert!(is_owner_only(&state).unwrap());
        let path = state.join("core-config.json");
        fs::write(&path, "{\"stale\": true}").unwrap();
        write_private_json(&path, &json!({"fresh": 1}), false).unwrap();
        assert!(is_owner_only(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"fresh\":1}");
        let log = state.join("runtime.log");
        fs::write(
            &log, "old
",
        )
        .unwrap();
        drop(private_log(&log).unwrap());
        assert!(is_owner_only(&log).unwrap());
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "old
"
        );
        // A folder is not a log file.
        fs::create_dir(state.join("folder.log")).unwrap();
        assert!(private_log(&state.join("folder.log")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn second_lock_holder_waits_for_the_first() {
        let dir = temp_dir("lock");
        let path = dir.join("runtime.lock");
        let first = acquire_lock(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                let _second = acquire_lock(&path).unwrap();
                tx.send(()).unwrap();
            })
        };
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        drop(first);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_runtime_exit_names_a_declared_local_model() {
        let log = Path::new("/state/runtime.log");
        let plain = exit_message("exit status: 1", log, None);
        assert_eq!(
            plain,
            "hybrid runtime exited (exit status: 1); see /state/runtime.log"
        );
        let declared = exit_message("exit status: 1", log, Some("z-ai/glm-5.3-flash"));
        assert!(declared.starts_with(&plain));
        assert!(
            declared.contains("z-ai/glm-5.3-flash")
                && declared.contains("hybrid.local_registry_model")
        );
    }

    #[test]
    fn settings_errors_name_the_settings_file() {
        let path = Path::new("/home/user/.config/rayline/settings.json");
        let error = settings_error(
            Some(path),
            "settings hybrid.routing.min_p_pass must be a number in (0, 1)".to_owned(),
        );
        assert_eq!(
            error.to_string(),
            "/home/user/.config/rayline/settings.json: settings hybrid.routing.min_p_pass must be a number in (0, 1)"
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
        let argv: Vec<OsString> = ["rayline", "--env=dev", "hybrid", "status"]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert!(matches!(
            crate::rayline_dispatch_for_argv(&argv),
            crate::RaylineDispatch::Usage(_)
        ));
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

    /// Every hybrid harness advertises the 1M harness window, the core's
    /// output cap and text-only input, never the hosted 32k output / image
    /// defaults the core refuses (#123).
    #[test]
    fn every_harness_advertises_the_admitted_limits() {
        // The launcher reads the fallback override from the process env.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let core = ModelCapabilities {
            context_window: 98_304,
            max_output_tokens: 4096,
            input_modalities: vec!["text".into()],
        };
        let capabilities = harness_capabilities(core);
        assert_eq!(capabilities.context_window, 1_000_000);
        let paths = Paths {
            dir: temp_dir("limits"),
        };
        let read = |relative: &str| fs::read_to_string(paths.dir.join(relative)).unwrap();
        for name in ["pi", "opencode", "omp", "hermes", "openclaw"] {
            harness_command(
                &paths,
                name,
                &[],
                "http://127.0.0.1:20815",
                "local-token",
                &capabilities,
            )
            .unwrap();
        }
        let json = |relative: &str| -> Value { serde_json::from_str(&read(relative)).unwrap() };
        let pi = &json("harness/pi/models.json")["providers"]["rayline"]["models"][0];
        assert_eq!(
            (&pi["contextWindow"], &pi["maxTokens"], &pi["input"]),
            (&json!(1_000_000), &json!(4096), &json!(["text"]))
        );
        let opencode = &json("harness/opencode/opencode.json")["provider"]["rayline"]["models"]["rayline-auto"];
        assert_eq!(
            opencode["limit"],
            json!({"context": 1_000_000, "output": 4096})
        );
        assert_eq!(opencode["modalities"]["input"], json!(["text"]));
        let openclaw = json("harness/openclaw/openclaw.json");
        for provider in openclaw["models"]["providers"]
            .as_object()
            .unwrap()
            .values()
        {
            let model = &provider["models"][0];
            assert_eq!(
                (
                    &model["contextWindow"],
                    &model["maxTokens"],
                    &model["input"]
                ),
                (&json!(1_000_000), &json!(4096), &json!(["text"]))
            );
        }
        let omp = read("harness/omp/models.yml");
        for line in [
            "contextWindow: 1000000",
            "maxTokens: 4096",
            "input: [\"text\"]",
        ] {
            assert!(omp.contains(line), "{omp}");
        }
        let hermes = read("harness/hermes/config.yaml");
        assert!(hermes.contains("context_length: 1000000"), "{hermes}");
        assert!(
            hermes.contains("provider: \"custom:rayline-resp\""),
            "{hermes}"
        );
        let claude = harness_command(
            &paths,
            "claude",
            &[],
            "http://127.0.0.1:20815",
            "local-token",
            &capabilities,
        )
        .unwrap();
        assert!(
            claude
                .get_envs()
                .any(|(key, value)| key == "CLAUDE_CODE_MAX_OUTPUT_TOKENS"
                    && value == Some(std::ffi::OsStr::new("4096")))
        );
        // `rayline-auto` is not in Claude Code's catalog, so the window it
        // may budget against is stated rather than assumed (200k).
        assert!(
            claude
                .get_envs()
                .any(|(key, value)| key == "CLAUDE_CODE_MAX_CONTEXT_TOKENS"
                    && value == Some(std::ffi::OsStr::new("1000000")))
        );
        // Auto mode's checks come from Claude Code's own classifier requests,
        // not from the API inside a turn the local model may serve; a value
        // the user set is kept.
        assert_eq!(auto_mode_server_default(None), Some("0"));
        for kept in ["0", "1", ""] {
            assert_eq!(
                auto_mode_server_default(Some(std::ffi::OsStr::new(kept))),
                None,
                "{kept:?}"
            );
        }
        let codex = harness_command(
            &paths,
            "codex",
            &[],
            "http://127.0.0.1:20815",
            "local-token",
            &capabilities,
        )
        .unwrap();
        let args: Vec<_> = codex.get_args().map(|arg| arg.to_string_lossy()).collect();
        assert!(
            args.iter().any(|arg| arg == "model_max_output_tokens=4096"),
            "{args:?}"
        );
        assert!(
            args.iter().any(|arg| arg == "model_context_window=1000000"),
            "{args:?}"
        );
        let _ = fs::remove_dir_all(&paths.dir);
    }

    /// Sets the fallback override for one scope and restores the prior value
    /// on drop, also while unwinding, so a failed assertion cannot leak `1`
    /// into later tests. Callers hold [`ENV_LOCK`].
    struct FallbackOverride(Option<std::ffi::OsString>);

    impl FallbackOverride {
        fn set(value: Option<&str>) -> Self {
            let prior = std::env::var_os(crate::harness::CONV_ID_FALLBACK_ENV);
            // SAFETY: serialized by ENV_LOCK among the tests that take it and
            // by the `--test-threads=1` gate for the rest; restored by Drop.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(crate::harness::CONV_ID_FALLBACK_ENV, value),
                    None => std::env::remove_var(crate::harness::CONV_ID_FALLBACK_ENV),
                }
            }
            Self(prior)
        }
    }

    impl Drop for FallbackOverride {
        fn drop(&mut self) {
            // SAFETY: serialized as above; ENV_LOCK is still held by the caller.
            unsafe {
                match &self.0 {
                    Some(prior) => std::env::set_var(crate::harness::CONV_ID_FALLBACK_ENV, prior),
                    None => std::env::remove_var(crate::harness::CONV_ID_FALLBACK_ENV),
                }
            }
        }
    }

    /// A hybrid launch keys the episode on the harness's own session id, so no
    /// config references the per-launch id and none is exported. Setting
    /// `RAYLINE_HARNESS_CONV_ID_FALLBACK=1` turns it on here as on the hosted
    /// path, and only then is the env var set.
    #[test]
    fn hybrid_conversation_id_fallback_is_opt_in() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let capabilities = harness_capabilities(ModelCapabilities {
            context_window: 98_304,
            max_output_tokens: 4096,
            input_modalities: vec!["text".into()],
        });
        let conv_env = std::ffi::OsStr::new(crate::harness::CONV_ID_ENV);
        let launch = |paths: &Paths, name: &str| {
            let command = harness_command(
                paths,
                name,
                &[],
                "http://127.0.0.1:20815",
                "local-token",
                &capabilities,
            )
            .unwrap();
            // Every file the launch wrote for this harness, so the assertion
            // does not depend on which one carries the header.
            let config: String = fs::read_dir(paths.dir.join("harness").join(name))
                .unwrap()
                .filter_map(|entry| fs::read_to_string(entry.unwrap().path()).ok())
                .collect();
            let exported = command
                .get_envs()
                .find(|(key, _)| *key == conv_env)
                .map(|(_, value)| value.is_some());
            (config, exported)
        };
        // Off: explicitly unset, so a value inherited from the developer's
        // shell cannot turn the fallback on. An exported-but-removed id
        // (`Some(false)`) shows an inherited id cannot leak in either.
        for (forced, sends) in [(None, false), (Some("1"), true)] {
            let _override = FallbackOverride::set(forced);
            for name in ["hermes", "pi", "opencode", "omp", "openclaw"] {
                let paths = Paths {
                    dir: temp_dir(&format!("conv-{}-{name}", if sends { "on" } else { "off" })),
                };
                let (config, exported) = launch(&paths, name);
                assert_eq!(
                    config.contains(crate::harness::CONV_HEADER),
                    sends,
                    "{name}: {config}"
                );
                assert_eq!(exported, Some(sends), "{name}");
                let _ = fs::remove_dir_all(&paths.dir);
            }
        }
    }

    #[test]
    fn decisions_probe_verdicts() {
        assert_eq!(probe_verdict(Ok(200)), ProbeVerdict::Ready);
        assert_eq!(probe_verdict(Ok(400)), ProbeVerdict::Ready);
        assert_eq!(probe_verdict(Ok(401)), ProbeVerdict::KeyRefused(401));
        assert_eq!(probe_verdict(Ok(403)), ProbeVerdict::KeyRefused(403));
        assert_eq!(probe_verdict(Ok(502)), ProbeVerdict::Unavailable);
        assert_eq!(probe_verdict(Err(())), ProbeVerdict::Unavailable);
        assert_eq!(probe_verdict(Ok(402)), ProbeVerdict::Rejected(402));
        assert_eq!(probe_verdict(Ok(404)), ProbeVerdict::Rejected(404));
        assert_eq!(probe_verdict(Ok(429)), ProbeVerdict::RateLimited);
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
            ("opencode", "-c"),
            ("opencode", "--continue"),
            ("opencode", "-s"),
            ("opencode", "--session=ses_1"),
            ("opencode", "--fork"),
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
