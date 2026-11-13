//! The local process (spec 3.3): `rayline local start|status|stop`, and the
//! connection a local-mode `rayline run` makes to it, starting it first when
//! none is running.
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use rayline_local_router::ModelCapabilities;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Settings;

/// How long `start` waits for a model's tokenizer files before it uses the
/// cached copy.
const TOKENIZER_FETCH_DEADLINE: Duration = Duration::from_secs(20);

/// The deadline for the core to initialize and report ready, including its
/// runtime profile checks, and for the Decisions readiness check.
const STARTUP_TIMEOUT_SECS: u64 = 120;

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
    fn lock(&self) -> PathBuf {
        self.dir.join("runtime.lock")
    }
}

/// `~/.rayline/local/`, the local process's state (spec 8.2).
pub(crate) fn state_dir(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR).join("local")
}

/// `~/.rayline/local/harness/<name>/`, apart from cloud mode's folder because
/// each mode writes its own endpoint, key and model list.
fn harness_dir(home: &Path, harness: crate::harness::Harness) -> PathBuf {
    state_dir(home).join("harness").join(harness.name())
}

/// What `supervisor.json` records about the running local process.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Supervisor {
    pid: u32,
    /// The config file it was started from, canonical.
    config: PathBuf,
    /// The `--env` it was started with. `None` is prod.
    env: Option<String>,
}

impl Supervisor {
    /// Refuses a run whose config file or `--env` is not the running
    /// process's (spec 3.3 item 3): it never runs on another config.
    fn check_serves(&self, settings: &Settings) -> io::Result<()> {
        let config = canonical_config(settings)?;
        if self.config != config {
            return Err(io::Error::other(format!(
                "the local process (pid {}) runs from {}, not {}; stop it with `rayline local stop` first",
                self.pid,
                self.config.display(),
                config.display()
            )));
        }
        if self.env != settings.env {
            return Err(io::Error::other(format!(
                "the local process (pid {}) was started with {}, and this run has {}; stop it with `rayline local stop` first",
                self.pid,
                env_words(self.env.as_deref()),
                env_words(settings.env.as_deref())
            )));
        }
        Ok(())
    }
}

/// `--env <name>` as an error names it.
fn env_words(env: Option<&str>) -> String {
    match env {
        Some(name) => format!("`--env {name}`"),
        None => "no `--env`".to_owned(),
    }
}

fn canonical_config(settings: &Settings) -> io::Result<PathBuf> {
    fs::canonicalize(&settings.file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{}: {error}", settings.file.display()),
        )
    })
}

/// `rayline local start`: start the local process from `settings`.
pub(crate) async fn start(settings: &Settings) -> ExitCode {
    exit_code(
        async {
            let (home, paths) = local_paths()?;
            let _lock = acquire_lock(&paths.lock())?;
            if let Some(running) = running(&paths)? {
                return Err(io::Error::other(format!(
                    "the local process is already running (pid {}, config {}); stop it with `rayline local stop` first",
                    running.pid,
                    running.config.display()
                )));
            }
            launch(&home, &paths, settings, &|line| println!("{line}")).await
        }
        .await,
    )
}

/// `rayline local status`.
pub(crate) async fn status() -> ExitCode {
    exit_code(
        async {
            let (_, paths) = local_paths()?;
            let Some(running) = running(&paths)? else {
                println!("Local process: not running");
                return Ok(());
            };
            let state = ready(&paths)?;
            health(&gateway_client()?, &paths, &state).await?;
            println!(
                "Local process: running (pid {})\nAddress: {}\nConfig: {}\nEnvironment: {}",
                running.pid,
                url(&state)?,
                running.config.display(),
                running.env.as_deref().unwrap_or("prod")
            );
            Ok(())
        }
        .await,
    )
}

/// `rayline local stop`: stop the gateway and the execution core. External
/// model servers keep running.
pub(crate) async fn stop() -> ExitCode {
    exit_code(
        async {
            let (_, paths) = local_paths()?;
            let _lock = acquire_lock(&paths.lock())?;
            if running(&paths)?.is_none() {
                clear_state(&paths)?;
                println!("Local process: not running");
                return Ok(());
            }
            let state = ready(&paths)?;
            // Shutdown remains available when core health fails. The private
            // gateway token authenticates this command independently.
            let shutdown: Value = gateway_client()?
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
                return Err(io::Error::other("local process shutdown identity mismatch"));
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while paths.ready().exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::other(format!(
                        "the local process did not stop within 10s; see {}",
                        paths.log().display()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            fs::remove_file(paths.supervisor())?;
            println!("Local process stopped.");
            Ok(())
        }
        .await,
    )
}

/// A local-mode `rayline run <harness>`: connect `harness` to the local
/// process running from `settings`, starting it if none is running.
pub(crate) async fn run_local(settings: &Settings, harness: &str, args: &[OsString]) -> ExitCode {
    match run_harness(settings, harness, args).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn exit_code(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The home directory and the local process's state directory, created
/// owner-only.
fn local_paths() -> io::Result<(PathBuf, Paths)> {
    require_supported_os()?;
    let home = crate::home().map_err(io::Error::other)?;
    let paths = Paths {
        dir: state_dir(&home),
    };
    private_dir(&paths.dir)?;
    Ok((home, paths))
}

async fn run_harness(
    settings: &Settings,
    harness: &str,
    args: &[OsString],
) -> io::Result<ExitCode> {
    require_local_args(harness, args)?;
    let (home, paths) = local_paths()?;
    {
        let _lock = acquire_lock(&paths.lock())?;
        match running(&paths)? {
            Some(running) => running.check_serves(settings)?,
            // Progress goes to stderr: the harness owns stdout.
            None => launch(&home, &paths, settings, &|line| eprintln!("{line}")).await?,
        }
    }
    let client = gateway_client()?;
    let state = ready(&paths)?;
    health(&client, &paths, &state).await?;
    let token = fs::read_to_string(paths.token())?;
    let mut command = harness_command(
        &home,
        harness,
        args,
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
    Ok(crate::child_exit_code(status))
}

fn gateway_client() -> io::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)
}

fn require_supported_os() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "cannot determine macOS version for the local process",
            ));
        }
        let version = String::from_utf8(output.stdout).map_err(io::Error::other)?;
        let major = version
            .trim()
            .split('.')
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| io::Error::other("invalid macOS version for the local process"))?;
        if major < 13 {
            return Err(io::Error::other(
                "the local process requires macOS 13 Ventura or later",
            ));
        }
    }
    Ok(())
}

/// Start the local process from `settings` and wait until it is ready.
/// The caller holds the runtime lock and has checked that none is running.
/// `say` reports progress.
async fn launch(
    home: &Path,
    paths: &Paths,
    settings: &Settings,
    say: &dyn Fn(String),
) -> io::Result<()> {
    clear_state(paths)?;
    let config_file = canonical_config(settings)?;
    let core = installed_binary("rayline-core")?;
    let routing = settings
        .routing
        .as_ref()
        .expect("a local-mode load has routing");
    let hosted = crate::status::hosted_environment(settings);
    let config =
        crate::hybrid_profile::core_config(settings, &hosted.base_url).map_err(io::Error::other)?;
    // The local process refuses Decisions anywhere but this router or a
    // loopback address; refuse a router it would refuse here, by name.
    rayline_local_router::decisions_url::check_config(&config, [hosted.base_url.as_str()])
        .map_err(|error| {
            io::Error::other(format!(
                "the router of environment {}: {error}",
                hosted.name()
            ))
        })?;
    // One counter per tokenizer, named for each deployment of its model.
    let mut token_counters = Vec::new();
    for (id, model) in &settings.models {
        let Some(repo) = &model.tokenizer else {
            continue;
        };
        let hub = rayline_hf::hf_cache_dir(home).map_err(io::Error::other)?;
        let snapshot = tokenizer_snapshot(&hub, repo).await.map_err(|error| {
            io::Error::other(format!(
                "{}: cannot load the tokenizer {repo} of models entry {id}: {error}",
                settings.file.display()
            ))
        })?;
        if snapshot.cached {
            say(format!(
                "Prompt counter for {id}: {repo}, the cached copy (Hugging Face did not serve the current one)"
            ));
        } else {
            say(format!("Prompt counter for {id}: {repo}"));
        }
        for &protocol in &settings.endpoints[&model.endpoint].protocols {
            token_counters.push((
                crate::hybrid_profile::deployment_id(id, protocol),
                snapshot.dir.clone(),
            ));
        }
    }
    // The environment's saved router key, minted on first use after
    // `rayline auth login`. It is the only key the core gets from Rayline;
    // every endpoint's own key comes from its `api_key_env`.
    let router_key = crate::status::router_key(&hosted, home)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    write_private_json(&paths.core_config(), &config, true)?;
    say(format!(
        "Decisions: {} via {}",
        routing.decisions_model, hosted.base_url
    ));
    say(format!("Routing policy: {}", routing.policy.preset));
    for (id, model) in &settings.models {
        say(format!(
            "Model {id}: {} at {}",
            model.model, settings.endpoints[&model.endpoint].base_url
        ));
    }
    say(format!(
        "Auto-mode classifier: {} (Claude Code's safety checks)",
        routing.classifier_model
    ));
    say(format!("Rayline key: the saved key for {}", hosted.name()));
    // One Decisions call before reporting ready: a refused key or an
    // unreachable router fails here, not on the harness's first turn.
    probe_decisions(&config, &router_key, settings.env.as_deref(), say).await?;
    let log = private_log(&paths.log())?;
    let mut command = Command::new(current_exe()?);
    command
        .arg(crate::cli::LOCAL_PROCESS)
        .arg("--core")
        .arg(core)
        .arg("--config")
        .arg(paths.core_config())
        .arg("--startup-timeout-secs")
        .arg(STARTUP_TIMEOUT_SECS.to_string())
        .arg("--token-file")
        .arg(paths.token())
        .arg("--ready-file")
        .arg(paths.ready())
        .arg("--router-url")
        .arg(&hosted.base_url)
        .arg("--classifier-model")
        .arg(&routing.classifier_model)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if let Some(env_name) = &settings.env {
        command.arg("--env-name").arg(env_name);
    }
    for (deployment, dir) in &token_counters {
        command.arg("--token-counter-dir").arg(deployment).arg(dir);
    }
    command.env(crate::hybrid_profile::ROUTER_KEY_ENV, &router_key);
    let mut child = crate::detach::spawn_detached(&mut command)?;
    let startup = async {
        let supervisor = Supervisor {
            pid: child.id(),
            config: config_file,
            env: settings.env.clone(),
        };
        write_private_json(
            &paths.supervisor(),
            &serde_json::to_value(&supervisor).map_err(io::Error::other)?,
            false,
        )?;
        let client = gateway_client()?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "the local process exited ({status}); see {}",
                    paths.log().display()
                )));
            }
            if paths.ready().exists() {
                let state = ready(paths)?;
                if state.get("pid").and_then(Value::as_u64) != Some(u64::from(child.id())) {
                    return Err(io::Error::other("local process startup identity mismatch"));
                }
                health(&client, paths, &state).await?;
                say(format!(
                    "Local process ready at {}\nLogs: {}",
                    url(&state)?,
                    paths.log().display()
                ));
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(STARTUP_TIMEOUT_SECS + 5) {
                return Err(io::Error::other(format!(
                    "the local process did not start within {STARTUP_TIMEOUT_SECS}s; see {}",
                    paths.log().display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    .await;
    if let Err(error) = startup {
        stop_spawned(&mut child).await?;
        clear_state(paths)?;
        return Err(error);
    }
    Ok(())
}

/// The snapshot of `repo`'s tokenizer and chat template, fetched from Hugging
/// Face within [`TOKENIZER_FETCH_DEADLINE`], else the cached copy.
async fn tokenizer_snapshot(
    hub: &Path,
    repo: &str,
) -> Result<crate::hybrid_tokenizer::Snapshot, String> {
    // A detached thread, not a blocking task: the runtime waits for blocking
    // tasks at exit, and the downloader retries for far longer than `start`
    // should run.
    let (sender, fetching) = tokio::sync::oneshot::channel();
    std::thread::spawn({
        let hub = hub.to_owned();
        let repo = repo.to_owned();
        move || {
            let _ = sender.send(crate::hybrid_tokenizer::fetch(&hub, &repo));
        }
    });
    match tokio::time::timeout(TOKENIZER_FETCH_DEADLINE, fetching).await {
        Ok(Ok(fetched)) => fetched,
        Ok(Err(_)) => Err("the tokenizer download stopped".to_owned()),
        Err(_) => crate::hybrid_tokenizer::cached_or(
            hub,
            repo,
            format!(
                "Hugging Face did not answer within {}s and nothing is cached",
                TOKENIZER_FETCH_DEADLINE.as_secs()
            ),
        ),
    }
}

/// Send one Decisions request with the key the core will use and classify
/// the answer: an empty body is enough, as the router authenticates before it
/// validates. Skipped for a loopback Decisions URL.
async fn probe_decisions(
    config: &Value,
    key: &str,
    env: Option<&str>,
    say: &dyn Fn(String),
) -> io::Result<()> {
    let env_flag = env_flag(env);
    let url = config["decisions"]["url"]
        .as_str()
        .expect("core_config always names the Decisions URL");
    // A loopback Decisions service is a local fixture or development
    // service, not a router this check can describe.
    if reqwest::Url::parse(url).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            rayline_local_router::decisions_url::is_loopback(&host.to_ascii_lowercase())
        })
    }) {
        return Ok(());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let deadline = Instant::now() + Duration::from_secs(STARTUP_TIMEOUT_SECS);
    let mut announced = false;
    loop {
        let outcome = match client
            .post(url)
            .bearer_auth(key)
            .json(&json!({}))
            .send()
            .await
        {
            Ok(response) => Ok(response.status().as_u16()),
            Err(error) => Err(error),
        };
        match probe_verdict(outcome.as_ref().copied().map_err(|_| ())) {
            ProbeVerdict::Ready => return Ok(()),
            ProbeVerdict::KeyRefused(status) => {
                return Err(io::Error::other(format!(
                    "Decisions at {url} refused the Rayline key (HTTP {status}). Run `rayline {env_flag}auth login`, then try again."
                )));
            }
            ProbeVerdict::RateLimited => {
                return Err(io::Error::other(format!(
                    "Decisions at {url} refused the readiness check with HTTP 429: the Rayline key is rate limited or out of quota; check your account, then retry"
                )));
            }
            ProbeVerdict::Rejected(status) => {
                return Err(io::Error::other(format!(
                    "Decisions at {url} answered HTTP {status} to a readiness check; the local process cannot start"
                )));
            }
            ProbeVerdict::Unavailable => {
                if Instant::now() >= deadline {
                    let detail = match &outcome {
                        Ok(status) => format!("HTTP {status}"),
                        Err(error) => error.to_string(),
                    };
                    return Err(io::Error::other(format!(
                        "Decisions at {url} is unreachable or not ready ({detail}); check your network, then retry"
                    )));
                }
                if !announced {
                    announced = true;
                    say(format!("Waiting for Decisions at {url}..."));
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// The `--env` a command for `env` takes, with its trailing space: none for
/// the built-in prod.
fn env_flag(env: Option<&str>) -> String {
    env.map(|env| format!("--env {env} ")).unwrap_or_default()
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
fn probe_verdict(outcome: Result<u16, ()>) -> ProbeVerdict {
    match outcome {
        Err(()) => ProbeVerdict::Unavailable,
        Ok(status) if (200..300).contains(&status) => ProbeVerdict::Ready,
        Ok(status @ (401 | 403)) => ProbeVerdict::KeyRefused(status),
        Ok(429) => ProbeVerdict::RateLimited,
        Ok(500 | 502 | 503 | 504) => ProbeVerdict::Unavailable,
        // Authenticated, then refused the empty readiness body: the key and
        // router work.
        // A 404/405 means the URL is not a Decisions route: rejected.
        Ok(400 | 409 | 422) => ProbeVerdict::Ready,
        Ok(status) => ProbeVerdict::Rejected(status),
    }
}

/// This `raydock`, with symlinks resolved: the local process runs from it.
fn current_exe() -> io::Result<PathBuf> {
    fs::canonicalize(std::env::current_exe()?)
}

/// `name`, installed beside [`current_exe`].
fn installed_binary(name: &str) -> io::Result<PathBuf> {
    let executable = current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("rayline executable has no parent directory"))?;
    let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    fs::canonicalize(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "installed {name} is missing at {}: {error}. Reinstall the complete release with `raydock update --force`",
                path.display()
            ),
        )
    })
}

fn ready(paths: &Paths) -> io::Result<Value> {
    let state: Value =
        serde_json::from_slice(&fs::read(paths.ready())?).map_err(io::Error::other)?;
    if state.get("contract_version") != Some(&json!(1)) || state.get("ready") != Some(&json!(true))
    {
        return Err(io::Error::other("incompatible local process metadata"));
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
        return Err(io::Error::other("invalid local process loopback endpoint"));
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
        .map_err(|_| io::Error::other("local process readiness is unavailable or invalid"))?;
    for key in ["pid", "core_pid"] {
        state
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| process_exists(*pid))
            .ok_or_else(|| io::Error::other("the local process is not live"))?;
    }
    let token = fs::read_to_string(paths.token())
        .map_err(|_| io::Error::other("local gateway credential is unavailable"))?;
    if token.trim().is_empty() {
        return Err(io::Error::other("local gateway credential is invalid"));
    }
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| io::Error::other("local gateway credential is invalid"))?;
    authorization.set_sensitive(true);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, authorization);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .default_headers(headers)
        .build()
        .map_err(|_| io::Error::other("cannot initialize the local gateway connection"))?;
    health(&client, &paths, &state).await.map_err(|_| {
        io::Error::other("local gateway health/authentication or process identity check failed")
    })?;
    let endpoint = url(&state)?.trim_end_matches('/');
    Ok((client, format!("{endpoint}/v1/router/top/snapshot"), state))
}

fn credential_env_names(state: &Value) -> io::Result<Vec<&str>> {
    let values = state
        .get("credential_env")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            io::Error::other("missing local process credential environment references")
        })?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|name| !name.is_empty() && !name.contains(['=', '\0']))
                .ok_or_else(|| {
                    io::Error::other("invalid local process credential environment reference")
                })
        })
        .collect()
}

fn model_capabilities(state: &Value) -> io::Result<ModelCapabilities> {
    let capabilities: ModelCapabilities = serde_json::from_value(
        state
            .get("capabilities")
            .ok_or_else(|| io::Error::other("missing local process model capabilities"))?
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
        return Err(io::Error::other("local process health/identity mismatch"));
    }
    Ok(health)
}

fn harness_command(
    home: &Path,
    name: &str,
    args: &[OsString],
    endpoint: &str,
    token: &str,
    capabilities: &ModelCapabilities,
) -> io::Result<Command> {
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
            let recovery = crate::claude::refusal_recovery_values();
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
                "model_provider=\"rayline-local\"".to_owned(),
                "model=\"rayline-auto\"".to_owned(),
                "model_providers.rayline-local.name=\"Rayline local\"".to_owned(),
                format!("model_providers.rayline-local.base_url=\"{endpoint}/v1\""),
                "model_providers.rayline-local.wire_api=\"responses\"".to_owned(),
                "model_providers.rayline-local.env_key=\"RAYLINE_HYBRID_GATEWAY_TOKEN\"".to_owned(),
                format!("model_context_window={}", capabilities.context_window),
                format!("model_max_output_tokens={}", capabilities.max_output_tokens),
            ] {
                command.arg("-c").arg(setting);
            }
        }
        _ => {
            let harness = crate::harness::Harness::from_command(name).ok_or_else(|| {
                io::Error::other(format!("unsupported local-mode harness {name:?}"))
            })?;
            let major_version = harness.major_version(&program).map_err(io::Error::other)?;
            let args = harness.hybrid_args(args, major_version);
            let config_dir = harness_dir(home, harness);
            private_dir(&config_dir)?;
            let ctx = crate::harness::RenderContext {
                key: token,
                router_url: endpoint,
                config_dir: &config_dir,
                capabilities: Some(capabilities),
                harness_major_version: major_version,
                models: crate::harness::LaunchModels::hybrid(capabilities),
            };
            let plan = harness.plan(&ctx).map_err(io::Error::other)?;
            crate::harness::write_files(&config_dir, &plan.files)?;
            command = crate::harness::build_command(&program, &plan, &args);
            command.env_remove("RAYLINE_EXEC_AUTH_TOKEN");
            return Ok(command);
        }
    }
    command.args(args);
    Ok(command)
}

/// Refuse a command line local mode can't run, before anything starts.
fn require_local_args(harness: &str, args: &[OsString]) -> io::Result<()> {
    require_fresh_session(harness, args)?;
    match crate::harness::Harness::from_command(harness) {
        Some(harness) => harness.require_hybrid_args(args),
        None => Ok(()),
    }
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
            "local mode requires a fresh harness session; saved sessions do not contain the core's private-append ledger",
        ));
    }
    Ok(())
}

/// The running local process's record, or `None` when none is running: no
/// record, or one whose process has exited.
fn running(paths: &Paths) -> io::Result<Option<Supervisor>> {
    let bytes = match fs::read(paths.supervisor()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if paths.ready().exists() || paths.token().exists() {
                return Err(io::Error::other(format!(
                    "{} holds local process state without {}; inspect it before removing it",
                    paths.dir.display(),
                    paths.supervisor().display()
                )));
            }
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let supervisor: Supervisor = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::other(format!("{}: {error}", paths.supervisor().display())))?;
    let pid = i32::try_from(supervisor.pid).map_err(|_| {
        io::Error::other(format!(
            "{}: invalid pid {}",
            paths.supervisor().display(),
            supervisor.pid
        ))
    })?;
    Ok(process_exists(pid).then_some(supervisor))
}

/// Remove what a local process that is no longer running left behind.
fn clear_state(paths: &Paths) -> io::Result<()> {
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
/// SIGTERM; Windows sets the stop event the local process waits on. `NotFound` means it
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

    #[test]
    fn a_missing_installed_binary_is_an_error_naming_its_path() {
        let name = "rayline-no-such-binary";
        let path = fs::canonicalize(std::env::current_exe().unwrap())
            .unwrap()
            .with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let error = installed_binary(name).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "installed {name} is missing at {}: ",
                path.display()
            )),
            "{message}"
        );
        assert!(
            message.ends_with("Reinstall the complete release with `raydock update --force`"),
            "{message}"
        );
    }

    // The built-in prod takes no `--env`: `--env prod` selects a configured
    // `environments.prod` instead.
    #[test]
    fn commands_for_an_environment_name_its_env() {
        assert_eq!(env_flag(None), "");
        assert_eq!(env_flag(Some("dev")), "--env dev ");
    }

    /// Serializes the tests that set a process-wide env var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Puts an `opencode` that reports version 2 first on `PATH`, so a hybrid
    /// OpenCode launch can read its version without OpenCode installed. The
    /// caller holds ENV_LOCK; Drop restores `PATH`.
    struct FakeOpenCode(Option<std::ffi::OsString>);

    impl FakeOpenCode {
        fn install() -> Self {
            let dir = temp_dir("fake-opencode");
            if cfg!(windows) {
                fs::write(dir.join("opencode.cmd"), "@echo opencode v2.0.25\r\n").unwrap();
            } else {
                let binary = dir.join("opencode");
                fs::write(&binary, "#!/bin/sh\necho 'opencode v2.0.25'\n").unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            let prior = std::env::var_os("PATH");
            let mut dirs = vec![dir];
            dirs.extend(prior.iter().flat_map(std::env::split_paths));
            // SAFETY: serialized by ENV_LOCK; restored by Drop.
            unsafe { std::env::set_var("PATH", std::env::join_paths(dirs).unwrap()) };
            Self(prior)
        }
    }

    impl Drop for FakeOpenCode {
        fn drop(&mut self) {
            // SAFETY: serialized as above; ENV_LOCK is still held by the caller.
            unsafe {
                match &self.0 {
                    Some(prior) => std::env::set_var("PATH", prior),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "rayline-local-{name}-{}-{unique}",
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

    /// Every file under `dir`, with its bytes.
    fn files_under(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(files_under(&path));
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
        files
    }

    /// Each mode writes its own endpoint, key and model list (spec 8.2), so a
    /// launch in one mode never rewrites what the other's harness reads.
    #[test]
    fn cloud_and_local_launches_write_apart() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _opencode = FakeOpenCode::install();
        let capabilities = ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 8192,
            input_modalities: vec!["text".into()],
        };
        for harness in crate::harness::Harness::ALL {
            let home = temp_dir(harness.name());
            let cloud_dir = crate::harness::config_dir(&home, harness);
            let local_dir = harness_dir(&home, harness);
            assert!(
                !cloud_dir.starts_with(&local_dir) && !local_dir.starts_with(&cloud_dir),
                "{harness:?}"
            );
            let cloud = || {
                let ctx = crate::harness::tests::render_context(&cloud_dir);
                let plan = harness.plan(&ctx).unwrap();
                crate::harness::write_files(&cloud_dir, &plan.files).unwrap();
            };
            let local = || {
                harness_command(
                    &home,
                    harness.name(),
                    &[],
                    "http://127.0.0.1:1",
                    "local-token",
                    &capabilities,
                )
                .unwrap();
            };
            cloud();
            let cloud_files = files_under(&cloud_dir);
            local();
            let local_files = files_under(&local_dir);
            assert!(!local_files.is_empty(), "{harness:?}");
            assert_eq!(files_under(&cloud_dir), cloud_files, "{harness:?}");
            cloud();
            assert_eq!(files_under(&local_dir), local_files, "{harness:?}");
            fs::remove_dir_all(&home).unwrap();
        }
    }

    #[test]
    fn cloud_credentials_are_not_exported_to_harnesses() {
        for name in ["claude", "codex"] {
            let command = harness_command(
                Path::new("/unused"),
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
        let _opencode = FakeOpenCode::install();
        let home = temp_dir("limits");
        let read = |relative: &str| fs::read_to_string(state_dir(&home).join(relative)).unwrap();
        for name in ["pi", "opencode", "omp", "hermes", "openclaw"] {
            harness_command(
                &home,
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
            &home,
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
            &home,
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
        let _ = fs::remove_dir_all(&home);
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

    /// The config file `name` in `dir`, with `routing` and an `environments.dev`
    /// entry, loaded in local mode for `env`.
    fn local_settings(dir: &Path, name: &str, env: Option<&str>) -> Settings {
        let file = dir.join(name);
        fs::write(
            &file,
            r#"{"routing": {"decisions_model": "arc-swift-1.0",
                "classifier_model": "anthropic/claude-haiku-5.5",
                "policy": {"preset": "arc-policy.hybrid-default.v1"}},
              "environments": {"dev": {"base_url": "https://router.example.test",
                "login_url": "https://router.example.test/cli-auth"}}}"#,
        )
        .unwrap();
        crate::config::load(&crate::config::LoadOptions {
            mode: crate::config::Mode::LocalFile(&file),
            env,
            harness_protocol: None,
        })
        .unwrap()
    }

    #[test]
    fn a_run_on_another_config_or_env_is_refused() {
        let dir = temp_dir("serves");
        let started = local_settings(&dir, "a.json", None);
        let supervisor = Supervisor {
            pid: 7,
            config: fs::canonicalize(&started.file).unwrap(),
            env: None,
        };
        supervisor.check_serves(&started).unwrap();

        let other = local_settings(&dir, "b.json", None);
        let error = supervisor.check_serves(&other).unwrap_err().to_string();
        for part in [
            "the local process (pid 7) runs from ",
            "a.json, not ",
            "b.json; stop it with `rayline local stop` first",
        ] {
            assert!(error.contains(part), "{error}");
        }

        let dev = local_settings(&dir, "a.json", Some("dev"));
        assert_eq!(
            supervisor.check_serves(&dev).unwrap_err().to_string(),
            "the local process (pid 7) was started with no `--env`, and this run has \
             `--env dev`; stop it with `rayline local stop` first"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn running_reads_the_record_of_a_live_process_only() {
        let dir = temp_dir("running");
        let paths = Paths { dir: dir.clone() };
        assert_eq!(running(&paths).unwrap(), None);

        fs::write(paths.ready(), "{}").unwrap();
        let error = running(&paths).unwrap_err().to_string();
        assert!(error.contains("supervisor.json"), "{error}");
        fs::remove_file(paths.ready()).unwrap();

        let record = |pid: u64| {
            fs::write(
                paths.supervisor(),
                json!({"pid": pid, "config": "/c.json", "env": "dev"}).to_string(),
            )
            .unwrap();
            running(&paths)
        };
        assert_eq!(
            record(u64::from(std::process::id())).unwrap(),
            Some(Supervisor {
                pid: std::process::id(),
                config: PathBuf::from("/c.json"),
                env: Some("dev".to_owned()),
            })
        );
        let error = record(u64::from(u32::MAX)).unwrap_err().to_string();
        assert!(error.contains("invalid pid"), "{error}");
        #[cfg(unix)]
        {
            let mut child = Command::new("true").spawn().unwrap();
            child.wait().unwrap();
            assert_eq!(record(u64::from(child.id())).unwrap(), None);
        }

        fs::write(
            paths.supervisor(),
            r#"{"pid": 1, "config": "/c.json", "env": null, "port": 9}"#,
        )
        .unwrap();
        let error = running(&paths).unwrap_err().to_string();
        assert!(error.contains("supervisor.json"), "{error}");
        let _ = fs::remove_dir_all(&dir);
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
