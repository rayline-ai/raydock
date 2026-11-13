//! `rayline run codex`: run Codex CLI on ARC through the Rayline router.
//!
//! Codex talks to the environment's router directly over the OpenAI Responses
//! API. Rayline gives it its own `CODEX_HOME` (`~/.rayline/harness/codex/`),
//! whose owner-only `config.toml` names the `rayline` provider, the model and
//! the router key. The key never goes in Codex's argv or environment.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexAuthMode {
    Auto,
    None,
    Subscription,
}

impl CodexAuthMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "none" => Some(Self::None),
            "subscription" => Some(Self::Subscription),
            _ => None,
        }
    }

    /// `auto` runs on the router with the user's key. The other modes fail.
    fn require_auto(self) -> Result<(), String> {
        match self {
            Self::Auto => Ok(()),
            Self::Subscription => Err("`--auth subscription` is not available yet".to_owned()),
            Self::None => Err("`--auth none` is not available yet".to_owned()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    /// The `--model` the user picked. `None` runs the key's main model.
    pub model: Option<String>,
    pub auth_mode: CodexAuthMode,
    pub codex_args: Vec<OsString>,
    /// The hosted environment the loaded config selects, whose router Codex
    /// talks to.
    pub hosted: crate::status::HostedEnvironment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigureRequest {
    /// See [`RunRequest::model`].
    pub model: Option<String>,
    pub auth_mode: CodexAuthMode,
    /// The hosted environment the loaded config selects, whose router Codex
    /// talks to.
    pub hosted: crate::status::HostedEnvironment,
}

/// The `rayline` provider Codex is configured with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Provider {
    /// The router's Responses base URL, ending in `/v1`.
    pub base_url: String,
    /// The `rlk-` router key Codex sends as its bearer token.
    pub key: String,
    pub model: String,
}

/// Resolve the provider for a Codex launch: the environment's router, the
/// stored `rlk-` router key (see [`crate::status::router_key`]) and the
/// model (`--model`, else the key's main model).
pub(crate) async fn resolve_provider(
    auth_mode: CodexAuthMode,
    model: Option<&str>,
    hosted: &crate::status::HostedEnvironment,
) -> Result<Provider, String> {
    auth_mode.require_auto()?;
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let router_url = crate::harness::normalize_router_url(&hosted.base_url);
    let key = crate::status::router_key(hosted, &home)
        .await
        .map_err(|error| error.to_string())?;
    let model = match model {
        Some(model) => model.to_owned(),
        None => {
            crate::harness::fetch_hosted_models(&router_url, &key)
                .await?
                .main
        }
    };
    Ok(Provider {
        base_url: format!("{router_url}/v1"),
        key,
        model,
    })
}

/// The `CODEX_HOME` `rayline run codex` runs Codex CLI on.
fn codex_home(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR)
        .join("harness")
        .join("codex")
}

pub async fn run(request: RunRequest) -> ExitCode {
    let provider = match resolve_provider(
        request.auth_mode,
        request.model.as_deref(),
        &request.hosted,
    )
    .await
    {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    let Some(home) = dirs::home_dir() else {
        eprintln!("Error: home directory not found");
        return ExitCode::from(1);
    };
    let codex_home = codex_home(&home);
    if let Err(error) =
        crate::codex_app::write_codex_home(&codex_home, &provider_config_toml(&provider))
    {
        eprintln!("Error: failed to prepare {}: {error}", codex_home.display());
        return ExitCode::from(1);
    }
    let mut command = codex_command();
    command.env("CODEX_HOME", &codex_home);
    command.args(request.codex_args);
    exec_or_status(&mut command)
}

/// Keep the router key out of Codex's environment (#84). Codex reads the key
/// from its owner-only `config.toml`, so it never needs it in the environment.
/// Codex snapshots its environment for the shells it spawns
/// (`$CODEX_HOME/shell_snapshots`), so a key it inherited would be written to
/// disk in plain text.
pub(crate) fn withhold_router_key(command: &mut Command) {
    command.env_remove("RAYLINE_ROUTER_API_KEY");
}

/// A `codex` child process with the router key withheld (#84). Every Codex
/// spawn goes through here (the run and the app launch), so none can inherit
/// the key, and each finds npm's `codex.cmd` on Windows.
pub(crate) fn codex_command() -> Command {
    codex_command_on(std::env::var_os("PATH"))
}

fn codex_command_on(path: Option<OsString>) -> Command {
    let mut command = Command::new(crate::harness::launch_program("codex", path));
    withhold_router_key(&mut command);
    command
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The Codex `config.toml` settings that point Codex at the `rayline` provider.
/// `rayline run codex` and `rayline run codex app` merge them into their `CODEX_HOME`'s
/// `config.toml`; `rayline run codex configure` writes them as the profile.
pub(crate) fn provider_config_toml(provider: &Provider) -> String {
    format!(
        "model = {}\nmodel_provider = \"rayline\"\n\n[model_providers.rayline]\nname = \"Rayline\"\nbase_url = {}\nwire_api = \"responses\"\nexperimental_bearer_token = {}\n",
        toml_string(&provider.model),
        toml_string(&provider.base_url),
        toml_string(&provider.key),
    )
}

/// Write the `rayline` profile to `$CODEX_HOME/rayline.config.toml` (Codex's
/// default home is `~/.codex`), owner-only since it holds the router key.
pub async fn configure(request: &ConfigureRequest) -> io::Result<String> {
    let provider = resolve_provider(request.auth_mode, request.model.as_deref(), &request.hosted)
        .await
        .map_err(io::Error::other)?;
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    std::fs::create_dir_all(&codex_home)?;
    let profile_path = codex_home.join("rayline.config.toml");
    crate::harness::write_private(&profile_path, &provider_config_toml(&provider))?;
    Ok(format!(
        "Wrote Codex Rayline profile: {}\nUse Codex profile `rayline`.\nBase URL: {}\n",
        profile_path.display(),
        provider.base_url
    ))
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    eprintln!("raydock: failed to exec codex: {error}");
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("raydock: failed to run codex: {error}");
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{CodexAuthMode, Provider, codex_command, codex_command_on, provider_config_toml};

    /// A provider as a launch resolves it, with a placeholder key.
    pub(crate) fn provider(model: &str) -> Provider {
        Provider {
            base_url: "https://api.rayline.ai/v1".to_owned(),
            key: "rlk-test-not-a-real-key".to_owned(),
            model: model.to_owned(),
        }
    }

    #[test]
    fn provider_config_points_codex_at_the_router_with_the_key() {
        let toml = provider_config_toml(&provider("arc-sage-1.0"));
        let doc: toml_edit::DocumentMut = toml.parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("arc-sage-1.0"));
        assert_eq!(doc["model_provider"].as_str(), Some("rayline"));
        let rayline = &doc["model_providers"]["rayline"];
        assert_eq!(
            rayline["base_url"].as_str(),
            Some("https://api.rayline.ai/v1")
        );
        assert_eq!(rayline["wire_api"].as_str(), Some("responses"));
        assert_eq!(
            rayline["experimental_bearer_token"].as_str(),
            Some("rlk-test-not-a-real-key")
        );
        for absent in ["requires_openai_auth", "forced_login_method", "127.0.0.1"] {
            assert!(!toml.contains(absent), "{absent}");
        }
    }

    #[test]
    fn only_auto_auth_runs() {
        assert!(CodexAuthMode::Auto.require_auto().is_ok());
        for mode in [CodexAuthMode::Subscription, CodexAuthMode::None] {
            let error = mode.require_auto().unwrap_err();
            assert!(error.contains("not available yet"), "{error}");
        }
    }

    #[tokio::test]
    async fn subscription_and_none_fail_before_reading_a_key() {
        for mode in [CodexAuthMode::Subscription, CodexAuthMode::None] {
            let hosted = crate::status::HostedEnvironment {
                env: None,
                base_url: "http://127.0.0.1:9".to_owned(),
                login_url: "http://127.0.0.1:9/cli-auth".to_owned(),
            };
            let error = super::resolve_provider(mode, None, &hosted)
                .await
                .unwrap_err();
            assert!(error.contains("not available yet"), "{error}");
        }
    }

    #[test]
    fn codex_writes_only_under_its_harness_folder() {
        let home = crate::harness::tests::temp_dir("codex-home");
        // The user's Codex home, with no config.toml to seed from.
        let user_home = crate::harness::tests::temp_dir("codex-home-user");
        let saved = std::env::var_os("CODEX_HOME");
        // SAFETY: the workspace tests run with --test-threads=1; restored below.
        unsafe { std::env::set_var("CODEX_HOME", &user_home) };
        let written = crate::codex_app::write_codex_home(
            &super::codex_home(&home),
            &provider_config_toml(&provider("arc-sage")),
        );
        // SAFETY: as above.
        match saved {
            Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }
        written.unwrap();
        crate::harness::tests::assert_only_harness_files(&home, "codex", &[]);
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(user_home).unwrap();
    }

    /// #84: every Codex child is built by `codex_command`, which withholds the
    /// router key, so no spawn site can forget it: the app launch included.
    #[test]
    fn every_codex_spawn_goes_through_codex_command() {
        let sources = [
            ("codex.rs", include_str!("codex.rs")),
            ("codex_app.rs", include_str!("codex_app.rs")),
        ];
        let count = |needle: &str| -> usize {
            sources
                .iter()
                .map(|(_, text)| text.matches(needle).count())
                .sum()
        };
        let bare = count(concat!("Command::new(", "\"codex\")"));
        assert_eq!(
            bare, 0,
            "a Codex child is spawned by bare name, which misses codex.cmd on Windows"
        );
        let total = count(concat!("launch_program(", "\"codex\""));
        assert_eq!(
            total, 1,
            "a Codex child is spawned outside codex_command (found {total} spawn sites)"
        );
    }

    /// Windows process creation does not apply PATHEXT, so a bare `codex`
    /// misses the `codex.cmd` launcher npm installs. The command resolves it
    /// the way `raydock hybrid` does, and keeps withholding the router key.
    #[test]
    fn codex_command_finds_the_npm_launcher_on_windows() {
        let dir = crate::harness::tests::temp_dir("codex-launcher");
        // npm installs an extensionless sh shim beside the `.cmd` launcher.
        std::fs::write(dir.join("codex"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("codex.cmd"), "@echo off\r\n").unwrap();
        let command = codex_command_on(Some(std::env::join_paths([&dir]).unwrap()));
        if cfg!(windows) {
            assert_eq!(command.get_program(), dir.join("codex.cmd").as_os_str());
        } else {
            assert_eq!(command.get_program(), "codex");
        }
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "RAYLINE_ROUTER_API_KEY" && value.is_none())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_command_withholds_the_router_key() {
        let command = codex_command();
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "RAYLINE_ROUTER_API_KEY" && value.is_none()),
            "codex_command does not remove RAYLINE_ROUTER_API_KEY"
        );
    }

    /// #84: a router key in the launcher's environment must not reach Codex,
    /// which snapshots its environment to disk for the shells it spawns.
    #[cfg(unix)]
    #[test]
    fn codex_child_never_inherits_the_router_key() {
        let var = "RAYLINE_ROUTER_API_KEY";
        let previous = std::env::var_os(var);
        unsafe { std::env::set_var(var, "rlk-test-not-a-real-key") };
        let mut command = std::process::Command::new("/usr/bin/env");
        super::withhold_router_key(&mut command);
        let output = command.output().expect("run env");
        match previous {
            Some(value) => unsafe { std::env::set_var(var, value) },
            None => unsafe { std::env::remove_var(var) },
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(
            !printed.contains(var),
            "the child environment still carries {var}"
        );
    }
}
