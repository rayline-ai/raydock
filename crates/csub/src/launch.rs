//! `csub claude`: run Claude Code against the running pool's gateway.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use rayline_subscriptions::CredentialStore;

use crate::app::Frontend;
use crate::gateway::LAUNCH_PREFIX;

/// Env vars that would take Claude Code off the gateway or its subscription
/// OAuth login (so its requests would no longer be pool-placed), provider
/// credentials it has no use for on the pool, and Rayline router keys it must
/// never see. Names checked against Claude Code 2.1.312.
pub(crate) const WITHHELD_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "RAYLINE_ROUTER_API_KEY",
    "RAYLINE_KEY",
    // Points Claude Code's secure storage (and so a stored Cloud gateway
    // sign-in) at another profile than the one checked here.
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
];

/// Settings files whose `env` block Claude Code applies over the process
/// environment. The gateway URL is also passed with `--settings`, which wins
/// over user and project settings but not over managed policy.
fn claude_config_dir() -> Option<PathBuf> {
    config_dir_override(std::env::var_os("CLAUDE_CONFIG_DIR"))
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

/// Claude Code tests `CLAUDE_CONFIG_DIR` for truthiness: empty means unset.
fn config_dir_override(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|dir| !dir.is_empty()).map(PathBuf::from)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Managed policy outranks our `--settings`, so it may not set the URL.
    Managed,
    /// User, project and local settings, and the global config's env block
    /// (applied first); our `--settings` overrides them in the foreground
    /// session.
    Overridable,
}

/// Project roots Claude Code reads `.claude/settings*.json` from: the working
/// directory, its git top level, and, in a linked worktree, the main checkout
/// (whose local settings Claude Code also applies). Every other worktree of
/// the repository is included too: `--resume`/`--continue` re-enter the
/// worktree a session was started in, after these checks.
fn project_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.to_owned()];
    // One value per call, so a path containing a newline stays intact.
    let rev_parse = |flag: &str| -> Option<PathBuf> {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "--path-format=absolute", flag])
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        let text = String::from_utf8_lossy(&output.stdout);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        (!text.is_empty()).then(|| PathBuf::from(text))
    };
    if let Some(top) = rev_parse("--show-toplevel") {
        roots.push(top);
    }
    // Only a standard `<checkout>/.git` names a main checkout; a bare or
    // separate git dir has no project settings beside it.
    if let Some(common) = rev_parse("--git-common-dir") {
        if common.file_name().is_some_and(|name| name == ".git") {
            if let Some(main) = common.parent() {
                roots.push(main.to_owned());
            }
        }
    }
    // NUL-separated where git supports it (2.36+), so a path containing a
    // newline stays intact; older git gets the line format.
    let list = |extra: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["worktree", "list", "--porcelain"])
            .args(extra)
            .output()
            .ok()
            .filter(|output| output.status.success())
    };
    let fields = list(&["-z"])
        .map(|output| (output.stdout, 0))
        .or_else(|| list(&[]).map(|output| (output.stdout, b'\n')));
    if let Some((stdout, separator)) = fields {
        for field in stdout.split(|byte| *byte == separator) {
            if let Some(path) = field.strip_prefix(b"worktree ") {
                roots.push(PathBuf::from(String::from_utf8_lossy(path).into_owned()));
            }
        }
    }
    let mut unique = Vec::with_capacity(roots.len());
    for root in roots {
        if !unique.contains(&root) {
            unique.push(root);
        }
    }
    unique
}

#[cfg(target_os = "macos")]
const MANAGED_DIR: &str = "/Library/Application Support/ClaudeCode";
#[cfg(not(target_os = "macos"))]
const MANAGED_DIR: &str = "/etc/claude-code";

fn settings_files(roots: &[PathBuf]) -> Vec<(PathBuf, Tier)> {
    let managed = Path::new(MANAGED_DIR);
    let mut files = vec![(managed.join("managed-settings.json"), Tier::Managed)];
    if let Ok(entries) = std::fs::read_dir(managed.join("managed-settings.d")) {
        let mut drop_ins = entries
            .filter_map(Result::ok)
            // Claude Code's filter: visible `*.json` files or symlinks.
            .filter(|entry| {
                entry
                    .file_type()
                    .is_ok_and(|kind| kind.is_file() || kind.is_symlink())
                    && !entry.file_name().to_string_lossy().starts_with('.')
            })
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        drop_ins.sort();
        files.extend(drop_ins.into_iter().map(|path| (path, Tier::Managed)));
    }
    if let Some(config_dir) = claude_config_dir() {
        files.push((config_dir.join("settings.json"), Tier::Overridable));
        // Claude Code prefers a legacy `.config.json` here when present;
        // both are checked rather than mirroring that choice.
        files.push((config_dir.join(".config.json"), Tier::Overridable));
    }
    // Claude Code tests the value's truthiness: empty means unset.
    let suffix =
        if std::env::var_os("CLAUDE_CODE_CUSTOM_OAUTH_URL").is_some_and(|url| !url.is_empty()) {
            "-custom-oauth"
        } else {
            ""
        };
    if let Some(dir) =
        config_dir_override(std::env::var_os("CLAUDE_CONFIG_DIR")).or_else(dirs::home_dir)
    {
        files.push((dir.join(format!(".claude{suffix}.json")), Tier::Overridable));
    }
    for root in roots {
        files.push((root.join(".claude/settings.json"), Tier::Overridable));
        files.push((root.join(".claude/settings.local.json"), Tier::Overridable));
    }
    files
}

/// MDM-delivered managed policy (macOS configuration profiles).
fn mdm_policies() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut paths = vec![PathBuf::from(
            "/Library/Managed Preferences/com.anthropic.claudecode.plist",
        )];
        // Claude Code uses the OS account name, not $USER.
        if let Some(user) = account_name() {
            paths.push(
                Path::new("/Library/Managed Preferences")
                    .join(user)
                    .join("com.anthropic.claudecode.plist"),
            );
        }
        paths.retain(|path| path.exists());
        paths
    }
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

#[cfg(target_os = "macos")]
fn account_name() -> Option<String> {
    // SAFETY: getuid has no preconditions. getpwuid returns null or a pointer
    // to static storage that stays valid until the next getpw* call; the name
    // is copied out before returning and nothing else here calls getpw*.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*entry).pw_name)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

fn read_settings(path: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    let text = if path.extension().is_some_and(|ext| ext == "plist") {
        let output = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(path)
            .output()
            .with_context(|| format!("read {}", path.display()))?;
        if !output.status.success() {
            bail!("{} cannot be read as a property list", path.display());
        }
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow!(error).context(format!("read {}", path.display())));
            }
        }
    };
    // Claude Code accepts a leading byte-order mark; an unreadable file is
    // refused rather than skipped, since it cannot be shown to be harmless.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    // Claude Code reads a blank file as `{}`.
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(text).map(Some).map_err(|_| {
        anyhow!(
            "{} is not valid JSON; csub cannot check its env block",
            path.display()
        )
    })
}

/// Refuses a launch whose settings would re-introduce a withheld variable or
/// move Claude Code off the gateway: managed policy outranks our
/// `--settings`, and background sessions may not receive it at all. Names
/// only, never values.
fn check_settings_env(files: &[(PathBuf, Tier)], background: bool) -> anyhow::Result<()> {
    for (path, tier) in files {
        let Some(settings) = read_settings(path)? else {
            continue;
        };
        // A policy helper's settings outrank ours and cannot be inspected
        // before launch.
        if *tier == Tier::Managed {
            for key in ["policyHelper", "policyHelpers"] {
                if settings.get(key).is_some() {
                    bail!(
                        "{} sets {key}, whose managed settings csub cannot check; launch without the pool",
                        path.display()
                    );
                }
            }
        }
        let Some(env) = settings.get("env").and_then(|env| env.as_object()) else {
            continue;
        };
        for name in env.keys() {
            let redirects = name == "ANTHROPIC_BASE_URL" && (*tier == Tier::Managed || background);
            if redirects || WITHHELD_ENV.contains(&name.as_str()) {
                bail!(
                    "{} sets {name} in its env block, which would take Claude Code off the subscription pool; remove it or launch without the pool",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

/// A resumed session re-enters the worktree it was started in. One made by a
/// `WorktreeCreate` hook is not registered with git, so `project_roots`
/// cannot find it; refuse resuming while such a hook is configured.
fn refuse_resume_into_hook_worktree(
    files: &[(PathBuf, Tier)],
    args: &[OsString],
) -> anyhow::Result<()> {
    let resumes = args.iter().filter_map(|arg| arg.to_str()).any(|arg| {
        let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
        matches!(flag, "--resume" | "--continue" | "--from-pr")
            || (!arg.starts_with("--") && arg.starts_with('-') && flag[1..].contains(['r', 'c']))
    });
    if !resumes {
        return Ok(());
    }
    for (path, _) in files {
        let Some(settings) = read_settings(path)? else {
            continue;
        };
        if settings
            .get("hooks")
            .and_then(|hooks| hooks.get("WorktreeCreate"))
            .is_some_and(|hook| !hook.is_null())
        {
            bail!(
                "{} configures a WorktreeCreate hook, and a resumed session may re-enter a worktree it made whose settings csub cannot check; start a new session or launch without the pool",
                path.display()
            );
        }
    }
    Ok(())
}

/// Flags that would replace or outrank the `--settings` the launcher adds.
/// Checked across every argument: an option value may itself be `--`.
fn refuse_settings_flags(args: &[OsString]) -> anyhow::Result<()> {
    for arg in args.iter().filter_map(|arg| arg.to_str()) {
        let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
        // --project-config-root (hidden) reads project settings from a
        // directory the checks above never see.
        if matches!(
            flag,
            "--settings" | "--managed-settings" | "--project-config-root"
        ) {
            bail!(
                "{flag} cannot be combined with the subscription pool, which passes its own --settings to pin the gateway URL"
            );
        }
        // Move the session to another directory after these checks ran on
        // this one; launch from that directory instead. `--tmux` needs
        // `--worktree`; `-w` may be bundled with other short flags.
        let short_worktree =
            !arg.starts_with("--") && arg.starts_with('-') && flag[1..].contains('w');
        if short_worktree || matches!(flag, "--deep-link-cwd-b64" | "--worktree" | "--tmux") {
            bail!(
                "{flag} cannot be combined with the subscription pool; launch from the target directory instead"
            );
        }
    }
    Ok(())
}

/// A stored Cloud gateway sign-in (`enterpriseGateway` in the profile's
/// credentials) makes Claude Code send inference to that gateway, whatever
/// `ANTHROPIC_BASE_URL` says.
async fn refuse_gateway_sign_in() -> anyhow::Result<()> {
    let Some(config_dir) = claude_config_dir().filter(|dir| dir.is_dir()) else {
        return Ok(());
    };
    let shown = config_dir.display().to_string();
    let signed_in = tokio::task::spawn_blocking(move || {
        CredentialStore::new(&config_dir)?.source_has_top_level_key("enterpriseGateway")
    })
    .await
    .context("check the Claude Code sign-in")?
    .map_err(|error| anyhow!("cannot check the Claude Code sign-in in {shown}: {error}"))?;
    if signed_in {
        bail!(
            "the Claude Code profile in {shown} is signed in to a Cloud gateway, which takes precedence over the subscription pool; run `claude auth logout` or launch without the pool"
        );
    }
    Ok(())
}

/// Checks that a csub daemon for `pool` answers on `port`, then replaces this
/// process with `claude` (spawns and waits where exec is unavailable).
pub async fn claude(
    frontend: &Frontend,
    pool: &str,
    port: u16,
    args: Vec<OsString>,
) -> anyhow::Result<ExitCode> {
    refuse_settings_flags(&args)?;
    let background = crate::supervisor::starts_background_session(&args);
    let cwd = std::env::current_dir().context("read the current directory")?;
    let mut files = settings_files(&project_roots(&cwd));
    files.extend(mdm_policies().into_iter().map(|path| (path, Tier::Managed)));
    check_settings_env(&files, background)?;
    refuse_resume_into_hook_worktree(&files, &args)?;
    refuse_gateway_sign_in().await?;
    if background {
        if let Some(config_dir) = claude_config_dir() {
            crate::supervisor::check(&config_dir, port).map_err(|message| anyhow!(message))?;
        }
    }
    ensure_daemon(frontend, pool, port).await?;
    let base_url = format!("http://127.0.0.1:{port}{LAUNCH_PREFIX}{}", new_launch_id());
    let settings = serde_json::json!({"env": {"ANTHROPIC_BASE_URL": base_url}}).to_string();
    let binary = std::env::var_os("CSUB_CLAUDE_BIN").unwrap_or_else(|| OsString::from("claude"));
    let mut command = Command::new(&binary);
    command
        .arg("--settings")
        .arg(&settings)
        .args(&args)
        .env("ANTHROPIC_BASE_URL", &base_url);
    for name in WITHHELD_ENV {
        command.env_remove(name);
    }
    let no_proxy = merged_no_proxy(
        std::env::var("NO_PROXY").ok().as_deref(),
        std::env::var("no_proxy").ok().as_deref(),
    );
    command
        .env("NO_PROXY", &no_proxy)
        .env("no_proxy", &no_proxy);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = command.exec();
        Err(anyhow!(error).context(format!("run {}", binary.to_string_lossy())))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("run {}", binary.to_string_lossy()))?;
        Ok(ExitCode::from(
            status.code().unwrap_or(1).clamp(0, 255) as u8
        ))
    }
}

async fn ensure_daemon(frontend: &Frontend, pool: &str, port: u16) -> anyhow::Result<()> {
    let name = frontend.name;
    let hint = format!("start it with `{name} service install` (or run `{name} serve`)");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        // Loopback only: an inherited HTTP(S)_PROXY must not carry this.
        .no_proxy()
        .build()
        .context("build HTTP client")?
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
        .map_err(|_| anyhow!("no subscription pool daemon answers on 127.0.0.1:{port}; {hint}"))?;
    let health: serde_json::Value = response
        .json()
        .await
        .map_err(|_| anyhow!("127.0.0.1:{port} is not a csub daemon; {hint}"))?;
    if health.get("service").and_then(|value| value.as_str()) != Some("csub") {
        bail!(
            "127.0.0.1:{port} is not a csub daemon (a legacy rld pool daemon has no gateway); {hint}"
        );
    }
    let served = health
        .get("pool")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if served != pool {
        bail!(
            "the csub daemon on 127.0.0.1:{port} serves pool {served:?}, not {pool:?}; set CSUB_PORT to the port of the daemon for {pool:?}"
        );
    }
    Ok(())
}

/// Both spellings carry the union of existing exceptions plus loopback, since
/// tools disagree on which one wins.
fn merged_no_proxy(upper: Option<&str>, lower: Option<&str>) -> String {
    let mut entries: Vec<&str> = Vec::new();
    for entry in [upper, lower]
        .into_iter()
        .flatten()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .chain(["127.0.0.1", "localhost"])
    {
        if !entry.is_empty() && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(",")
}

fn new_launch_id() -> String {
    format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_dir_is_unset() {
        assert_eq!(config_dir_override(None), None);
        assert_eq!(config_dir_override(Some(OsString::new())), None);
        assert_eq!(
            config_dir_override(Some(OsString::from("/c"))),
            Some(PathBuf::from("/c"))
        );
    }

    #[test]
    fn launch_ids_are_valid_gateway_path_segments() {
        let id = new_launch_id();
        assert_eq!(id.len(), 32);
        let path = format!("{LAUNCH_PREFIX}{id}/v1/messages");
        assert_eq!(
            crate::gateway::split_launch(&path),
            Some((Some(id.as_str()), "/v1/messages"))
        );
        assert_ne!(id, new_launch_id());
    }

    #[test]
    fn no_proxy_keeps_both_spellings_and_adds_loopback() {
        assert_eq!(
            merged_no_proxy(Some("corp.internal"), None),
            "corp.internal,127.0.0.1,localhost"
        );
        assert_eq!(
            merged_no_proxy(Some("a, b"), Some("b,c,localhost")),
            "a,b,c,localhost,127.0.0.1"
        );
        assert_eq!(merged_no_proxy(None, None), "127.0.0.1,localhost");
    }

    #[test]
    fn settings_that_reintroduce_a_withheld_variable_are_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let user = temp.path().join("settings.json");
        let managed = temp.path().join("managed-settings.json");
        let files = [
            (managed.clone(), Tier::Managed),
            (user.clone(), Tier::Overridable),
        ];
        check_settings_env(&files, false).expect("no settings files");
        // A user/project gateway URL is overridden by our --settings in the
        // foreground, but a background session may not receive it.
        std::fs::write(
            &user,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example","OTHER":"1"}}"#,
        )
        .expect("settings");
        check_settings_env(&files, false).expect("base URL is pinned by --settings");
        assert!(check_settings_env(&files, true).is_err());
        // Managed policy outranks --settings.
        std::fs::write(&managed, r#"{"env":{"ANTHROPIC_BASE_URL":"https://gw"}}"#)
            .expect("managed");
        assert!(check_settings_env(&files, false).is_err());
        std::fs::remove_file(&managed).expect("rm");
        // A byte-order mark does not hide a withheld variable.
        std::fs::write(
            &user,
            "\u{feff}{\"env\":{\"ANTHROPIC_API_KEY\":\"sk-test\"}}",
        )
        .expect("settings");
        let error = check_settings_env(&files, false).expect_err("api key");
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"), "{error}");
        assert!(!error.to_string().contains("sk-test"), "{error}");
        // Unparseable settings are refused, not skipped; blank ones are `{}`.
        std::fs::write(&user, "{not json").expect("settings");
        assert!(check_settings_env(&files, false).is_err());
        std::fs::write(&user, "\u{feff} \n").expect("settings");
        check_settings_env(&files, false).expect("blank settings");
    }

    #[test]
    fn resuming_is_refused_only_with_a_worktree_create_hook() {
        let temp = tempfile::tempdir().expect("tempdir");
        let settings = temp.path().join("settings.json");
        let files = vec![(settings.clone(), Tier::Overridable)];
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        std::fs::write(&settings, r#"{"hooks":{"Stop":[]}}"#).expect("settings");
        refuse_resume_into_hook_worktree(&files, &args(&["--continue"])).expect("no hook");
        std::fs::write(&settings, r#"{"hooks":{"WorktreeCreate":[]}}"#).expect("settings");
        refuse_resume_into_hook_worktree(&files, &args(&["-p", "hi"])).expect("not resuming");
        for flag in ["--continue", "--resume=abc", "-r", "-c", "--from-pr"] {
            assert!(
                refuse_resume_into_hook_worktree(&files, &args(&[flag])).is_err(),
                "{flag}"
            );
        }
    }

    #[test]
    fn global_config_and_policy_helpers_are_checked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join(".claude.json");
        let files = vec![(global.clone(), Tier::Overridable)];
        std::fs::write(&global, r#"{"numStartups":3}"#).expect("global");
        check_settings_env(&files, false).expect("no env block");
        std::fs::write(&global, r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#).expect("global");
        assert!(check_settings_env(&files, false).is_err());
        // Our --settings outranks its URL in the foreground only.
        std::fs::write(&global, r#"{"env":{"ANTHROPIC_BASE_URL":"http://x"}}"#).expect("global");
        check_settings_env(&files, false).expect("foreground");
        assert!(check_settings_env(&files, true).is_err());

        let managed = temp.path().join("managed-settings.json");
        std::fs::write(
            &managed,
            r#"{"policyHelpers":{"default":{"env":{"ANTHROPIC_BASE_URL":"http://x"}}}}"#,
        )
        .expect("managed");
        let error = check_settings_env(&[(managed.clone(), Tier::Managed)], false)
            .expect_err("policy helper");
        assert!(error.to_string().contains("policyHelpers"), "{error}");
        // Only managed sources run policy helpers.
        check_settings_env(&[(managed, Tier::Overridable)], false).expect("user tier");
    }

    #[test]
    fn project_settings_come_from_the_given_roots_only() {
        let files = settings_files(&[PathBuf::from("/a/b"), PathBuf::from("/main")]);
        assert!(files.contains(&(
            PathBuf::from("/a/b/.claude/settings.json"),
            Tier::Overridable
        )));
        assert!(files.contains(&(
            PathBuf::from("/main/.claude/settings.local.json"),
            Tier::Overridable
        )));
        assert!(!files.iter().any(|(path, _)| path.starts_with("/a/.claude")));
    }

    #[test]
    fn a_linked_worktree_includes_its_main_checkout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let main = temp.path().join("main");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", main.to_str().expect("utf8")]);
        git(&[
            "-C",
            main.to_str().expect("utf8"),
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        let linked = temp.path().join("linked");
        git(&[
            "-C",
            main.to_str().expect("utf8"),
            "worktree",
            "add",
            "-q",
            linked.to_str().expect("utf8"),
        ]);
        let roots = project_roots(&linked);
        let main = main.canonicalize().expect("main");
        assert!(
            roots
                .iter()
                .any(|root| root.canonicalize().ok() == Some(main.clone())),
            "{roots:?}"
        );
        // From the main checkout, a resumed session may re-enter the linked one.
        let linked = linked.canonicalize().expect("linked");
        let roots = project_roots(&main);
        assert!(
            roots
                .iter()
                .any(|root| root.canonicalize().ok() == Some(linked.clone())),
            "{roots:?}"
        );
    }

    #[test]
    fn settings_flags_are_refused_anywhere() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(refuse_settings_flags(&args(&["--settings=x.json"])).is_err());
        assert!(refuse_settings_flags(&args(&["--managed-settings", "{}"])).is_err());
        assert!(refuse_settings_flags(&args(&["--project-config-root=/other"])).is_err());
        assert!(refuse_settings_flags(&args(&["--deep-link-cwd-b64", "L3Q="])).is_err());
        assert!(refuse_settings_flags(&args(&["--deep-link-cwd-b64=L3Q="])).is_err());
        for flag in ["-w", "-w=x", "-pw", "--worktree", "--worktree=x", "--tmux"] {
            assert!(refuse_settings_flags(&args(&[flag])).is_err(), "{flag}");
        }
        refuse_settings_flags(&args(&["-p", "--model", "opus", "write it"])).expect("plain");
        assert!(
            refuse_settings_flags(&args(&["--system-prompt", "--", "--settings", "{}"])).is_err()
        );
        refuse_settings_flags(&args(&["--model", "opus"])).expect("other flags");
    }

    #[tokio::test]
    async fn refuses_to_launch_without_a_csub_daemon() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let error = claude(&crate::app::CSUB, "default", port, Vec::new())
            .await
            .expect_err("no daemon");
        assert!(
            error.to_string().contains("csub service install"),
            "{error}"
        );
    }
}
