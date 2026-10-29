//! `csub service install|uninstall|status`: run `csub serve` under launchd.
//!
//! The job runs the csub binary itself. `serve` takes the pool lock and exits
//! with [`crate::serve::EXIT_POOL_OWNED`] when another process owns the pool,
//! so launchd's KeepAlive retries never produce a second owner.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, anyhow, bail};

pub const LABEL: &str = "ai.rayline.csub";

pub struct ServiceOptions {
    pub pool_id: String,
    pub config_path: Option<PathBuf>,
    pub port: u16,
}

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/csub.log")
}

/// Renders the LaunchAgent for `binary` (an absolute path).
pub fn render_plist(binary: &Path, home: &Path, options: &ServiceOptions) -> String {
    let mut arguments = vec![
        binary.display().to_string(),
        "serve".to_owned(),
        "--pool".to_owned(),
        options.pool_id.clone(),
        "--port".to_owned(),
        options.port.to_string(),
    ];
    if let Some(config) = &options.config_path {
        arguments.push("--config".to_owned());
        arguments.push(config.display().to_string());
    }
    let mut program_arguments = String::new();
    for argument in &arguments {
        let _ = writeln!(
            program_arguments,
            "    <string>{}</string>",
            xml_escape(argument)
        );
    }
    let log = xml_escape(&log_path(home).display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{program_arguments}  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key><string>info</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>ExitTimeOut</key><integer>60</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn gui_domain() -> Result<String, anyhow::Error> {
    let output = Command::new("id").arg("-u").output().context("run id -u")?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if uid.is_empty() {
        bail!("could not determine the user id");
    }
    Ok(format!("gui/{uid}"))
}

fn launchctl(args: &[&str]) -> Result<std::process::Output, anyhow::Error> {
    Command::new("launchctl")
        .args(args)
        .output()
        .with_context(|| format!("run launchctl {}", args.join(" ")))
}

pub fn install(options: &ServiceOptions) -> Result<String, anyhow::Error> {
    if !cfg!(target_os = "macos") {
        bail!(
            "csub service install supports macOS launchd only; run `csub serve` under your init system"
        );
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let options = &ServiceOptions {
        pool_id: options.pool_id.clone(),
        // launchd runs the job from /, so a relative registry path must be
        // resolved now. A missing file fails here rather than in a loop.
        config_path: options
            .config_path
            .as_deref()
            .map(|path| {
                path.canonicalize()
                    .with_context(|| format!("resolve registry path {}", path.display()))
            })
            .transpose()?,
        port: options.port,
    };
    let binary = std::env::current_exe()
        .context("locate the csub binary")?
        .canonicalize()
        .context("resolve the csub binary path")?;
    let plist = plist_path(&home);
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let domain = gui_domain()?;
    let target = format!("{domain}/{LABEL}");
    // Replace a previous definition so a moved binary or new flags apply.
    let _ = launchctl(&["bootout", &target]);
    std::fs::write(&plist, render_plist(&binary, &home, options))
        .with_context(|| format!("write {}", plist.display()))?;
    let plist_arg = plist.display().to_string();
    let output = launchctl(&["bootstrap", &domain, &plist_arg])?;
    if !output.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(format!(
        "Installed {LABEL} running {} serve --pool {} on 127.0.0.1:{}.\nPlist: {}\nLog: {}\n",
        binary.display(),
        options.pool_id,
        options.port,
        plist.display(),
        log_path(&home).display()
    ))
}

pub fn uninstall() -> Result<String, anyhow::Error> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let domain = gui_domain()?;
    let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    let plist = plist_path(&home);
    match std::fs::remove_file(&plist) {
        Ok(()) => Ok(format!("Removed {LABEL} ({}).\n", plist.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(format!("{LABEL} was not installed.\n"))
        }
        Err(error) => Err(anyhow::Error::new(error).context(format!("remove {}", plist.display()))),
    }
}

pub fn status() -> Result<String, anyhow::Error> {
    let domain = gui_domain()?;
    let output = launchctl(&["print", &format!("{domain}/{LABEL}")])?;
    if !output.status.success() {
        return Ok(format!("{LABEL} is not loaded.\n"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut summary = format!("{LABEL} is loaded.\n");
    for line in text.lines().map(str::trim) {
        if line.starts_with("state =")
            || line.starts_with("pid =")
            || line.starts_with("last exit code =")
        {
            let _ = writeln!(summary, "  {line}");
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_csub_serve_directly() {
        let plist = render_plist(
            Path::new("/opt/csub/bin/csub"),
            Path::new("/Users/someone"),
            &ServiceOptions {
                pool_id: "default".to_owned(),
                config_path: Some(PathBuf::from("/Users/someone/a&b.json")),
                port: 20816,
            },
        );
        assert!(plist.contains("<string>/opt/csub/bin/csub</string>\n    <string>serve</string>"));
        assert!(plist.contains("<string>--pool</string>\n    <string>default</string>"));
        assert!(plist.contains("<string>/Users/someone/a&amp;b.json</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(plist.contains("/Users/someone/Library/Logs/csub.log"));
    }
}
