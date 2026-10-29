//! Claude Code's argument list as its parser (Commander) reads it, so launch
//! checks see real options rather than words inside prompts or option values.

use std::ffi::OsString;

/// Long options that take a required value, across Claude Code 2.1.312's
/// commands: the next argument is that value, not an option. (`--remote`
/// takes an optional value at the top level, so it is left out.)
const VALUE_OPTIONS: &[&str] = &[
    "--ablation",
    "--accept-command",
    "--add-dir",
    "--advisor",
    "--agent",
    "--agent-color",
    "--agent-id",
    "--agent-name",
    "--agent-type",
    "--agents",
    "--allow-tools",
    "--append-subagent-system-prompt",
    "--append-subagent-system-prompt-file",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--attach-serve",
    "--betas",
    "--channels",
    "--client-data-url",
    "--config",
    "--correlation-id",
    "--cwd",
    "--dangerously-load-development-channels",
    "--debug-file",
    "--deep-link-cwd-b64",
    "--deep-link-last-fetch",
    "--deep-link-repo",
    "--disallow-tools",
    "--effort",
    "--env",
    "--fallback-model",
    "--fast-mode",
    "--file",
    "--forward-home-settings",
    "--header",
    "--inherit-permission-mode",
    "--input-format",
    "--json-schema",
    "--managed-settings",
    "--max-budget-usd",
    "--max-cost-usd",
    "--max-thinking-tokens",
    "--max-turns",
    "--mcp-config",
    "--message",
    "--messaging-socket-path",
    "--model",
    "--name",
    "--output-format",
    "--parent-session-id",
    "--permission-mode",
    "--permission-prompt-tool",
    "--permission-prompts",
    "--plan-mode-instructions",
    "--plugin-dir",
    "--plugin-dir-no-mcp",
    "--plugin-url",
    "--prefill",
    "--prefill-b64",
    "--proactivity",
    "--project-config-root",
    "--remote-control-session-name-prefix",
    "--resume-drops-turn",
    "--resume-session-at",
    "--rewind-files",
    "--sdk-url",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-file",
    "--system-prompt-snapshot",
    "--task-budget",
    "--team-name",
    "--teammate-mode",
    "--thinking",
    "--thinking-display",
    "--tools",
    "--watch-artifact",
    "--watch-artifact-no-autoreact",
    "--workload",
];

/// Short options with a required value: last in a cluster, they take the
/// next argument. Optional-value ones (`-d`, `-r`, `-w`) take the rest of
/// their cluster; either way the rest of the cluster is a value.
const SHORT_REQUIRED: &[char] = &['e', 'H', 'j', 'm', 'n', 's', 't'];
const SHORT_OPTIONAL: &[char] = &['d', 'r', 'w'];

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClaudeArgs {
    /// Long options without `=value`, and each short flag of a cluster as
    /// `-x`. Everything after `--` is positional.
    pub options: Vec<String>,
    /// The first positional argument: a subcommand or the prompt.
    pub first_positional: Option<String>,
}

impl ClaudeArgs {
    pub fn has(&self, option: &str) -> bool {
        self.options.iter().any(|seen| seen == option)
    }
}

pub(crate) fn parse(args: &[OsString]) -> ClaudeArgs {
    let mut parsed = ClaudeArgs::default();
    let mut args = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = args.next() {
        if arg == "--" {
            if parsed.first_positional.is_none() {
                parsed.first_positional = args.next().map(|arg| arg.into_owned());
            }
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline_value) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            let name = format!("--{name}");
            if !inline_value && VALUE_OPTIONS.contains(&name.as_str()) {
                args.next();
            }
            parsed.options.push(name);
        } else if arg.len() > 1 && arg.starts_with('-') {
            let mut cluster = arg[1..].chars();
            while let Some(short) = cluster.next() {
                parsed.options.push(format!("-{short}"));
                if SHORT_OPTIONAL.contains(&short) {
                    break;
                }
                if SHORT_REQUIRED.contains(&short) {
                    if cluster.as_str().is_empty() {
                        args.next();
                    }
                    break;
                }
            }
        } else if parsed.first_positional.is_none() {
            parsed.first_positional = Some(arg.into_owned());
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(values: &[&str]) -> ClaudeArgs {
        parse(&values.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn clusters_split_until_a_value_taking_flag() {
        let parsed = parse_strs(&["-pw", "-cr", "abc"]);
        assert_eq!(parsed.options, ["-p", "-w", "-c", "-r"]);
        assert_eq!(parsed.first_positional.as_deref(), Some("abc"));
        // `-n` takes the next argument, so `-w` there is a value.
        assert!(!parse_strs(&["-n", "-w"]).has("-w"));
        // `-r` takes the rest of its cluster.
        assert!(!parse_strs(&["-rwx"]).has("-w"));
    }

    #[test]
    fn option_values_and_prompt_text_are_not_options() {
        let parsed = parse_strs(&["--append-system-prompt", "-w", "--model=opus", "-p", "logs"]);
        assert_eq!(parsed.options, ["--append-system-prompt", "--model", "-p"]);
        assert_eq!(parsed.first_positional.as_deref(), Some("logs"));
        let parsed = parse_strs(&["--name", "logs", "do the --worktree thing"]);
        assert!(!parsed.has("--worktree"));
        assert_eq!(
            parsed.first_positional.as_deref(),
            Some("do the --worktree thing")
        );
        let parsed = parse_strs(&["--", "-w"]);
        assert!(parsed.options.is_empty());
        assert_eq!(parsed.first_positional.as_deref(), Some("-w"));
    }
}
