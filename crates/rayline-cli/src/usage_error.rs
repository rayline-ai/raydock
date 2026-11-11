//! Error messages for rejected command lines.
//!
//! The argument parsers return `None` on any rejection. [`TrackedArgs`]
//! records how far they read, so the message can name the argument parsing
//! stopped at and show the usage line of the command it belongs to.

use std::cell::Cell;
use std::ffi::OsString;

use crate::RaylineDispatch;

/// Commands that exist only in non-public builds; only these report that they
/// are not available here.
const PRIVATE_COMMANDS: &[&str] = &["internal", "install-cli", "hook-send", "agent", "chat"];

pub(crate) const HYBRID_ROOT_OPTIONS: &str = "\
raydock: `hybrid` reads `--env` after its subcommand (`raydock hybrid start --env dev`) \
and has no `--auth-token`
Run `raydock hybrid --help` for details.";

/// The arguments after the program name, counting how many a parser has
/// taken and whether it asked for more than there were.
pub(crate) struct TrackedArgs<'a, 'c> {
    args: std::slice::Iter<'a, OsString>,
    taken: &'c Cell<usize>,
    exhausted: &'c Cell<bool>,
}

impl<'a, 'c> TrackedArgs<'a, 'c> {
    pub(crate) fn new(
        original_argv: &'a [OsString],
        taken: &'c Cell<usize>,
        exhausted: &'c Cell<bool>,
    ) -> Self {
        Self {
            args: original_argv.get(1..).unwrap_or_default().iter(),
            taken,
            exhausted,
        }
    }
}

impl<'a> Iterator for TrackedArgs<'a, '_> {
    type Item = &'a OsString;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.args.next();
        if next.is_some() {
            self.taken.set(self.taken.get() + 1);
        } else {
            self.exhausted.set(true);
        }
        next
    }
}

/// Explains why the parser rejected `original_argv`. `taken` and `exhausted`
/// come from the [`TrackedArgs`] the parser read: unless it ran out of
/// arguments, the last one it took is the one it stopped at.
pub(crate) fn rejected_argv(
    original_argv: &[OsString],
    taken: usize,
    exhausted: bool,
) -> RaylineDispatch {
    let args = original_argv
        .iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    let args = args.iter().map(AsRef::as_ref).collect::<Vec<&str>>();
    let stop = if exhausted {
        None
    } else {
        taken.checked_sub(1)
    };
    let command = command_path(&args, stop.unwrap_or(args.len()));

    let Some(stop) = stop else {
        let message = if command.is_empty() {
            "raydock: missing command".to_owned()
        } else {
            format!(
                "raydock: missing or conflicting arguments for `raydock {}`",
                command.join(" ")
            )
        };
        return RaylineDispatch::Usage(with_usage(message, &command));
    };
    let bad = args[stop];

    if let Some(env) = env_value_at(&args, stop) {
        if !crate::status::is_valid_root_env(env) {
            let message = format!("raydock: invalid environment name `{env}`");
            return RaylineDispatch::Usage(with_usage(message, &command));
        }
    }
    if command.is_empty() {
        if PRIVATE_COMMANDS.contains(&bad) {
            return RaylineDispatch::Unavailable(bad.to_owned());
        }
        let message = if bad.starts_with('-') {
            format!("raydock: unknown option `{bad}`")
        } else {
            format!("raydock: unknown command `{bad}`")
        };
        return RaylineDispatch::Usage(with_usage(message, &command));
    }
    if command[0] == "claude" && crate::is_claude_management_subcommand(bad) {
        return RaylineDispatch::Unavailable(format!("claude {bad}"));
    }
    if bad == "--env" || bad.starts_with("--env=") {
        return RaylineDispatch::Usage(misplaced_env(&args, stop));
    }
    let message = format!(
        "raydock: `raydock {}` does not accept `{bad}`",
        command.join(" ")
    );
    RaylineDispatch::Usage(with_usage(message, &command))
}

/// The command words in `args[..end]`: the first word after the root options
/// and the words that follow it up to the next option.
fn command_path<'a>(args: &[&'a str], end: usize) -> Vec<&'a str> {
    let mut command = Vec::new();
    let mut index = 0;
    while index < end {
        let arg = args[index];
        index += 1;
        if command.is_empty() {
            if crate::is_value_option(arg) && !arg.contains('=') {
                index += 1;
            } else if !arg.starts_with('-') {
                command.push(arg);
            }
        } else if arg.starts_with('-') {
            break;
        } else {
            command.push(arg);
        }
    }
    command
}

/// The `--env` value at `index`, as `--env=VALUE` or as the word after `--env`.
fn env_value_at<'a>(args: &[&'a str], index: usize) -> Option<&'a str> {
    args[index]
        .strip_prefix("--env=")
        .or_else(|| (index > 0 && args[index - 1] == "--env").then_some(args[index]))
}

/// `--env` after a command that does not read it: show the command with
/// `--env` moved in front, where every command reads it.
fn misplaced_env(args: &[&str], index: usize) -> String {
    let (value, len) = match args[index].strip_prefix("--env=") {
        Some(value) => (value, 1),
        None => (args.get(index + 1).copied().unwrap_or("<name>"), 2),
    };
    let rest = args
        .iter()
        .enumerate()
        .filter(|(position, _)| *position < index || *position >= index + len)
        .map(|(_, arg)| *arg)
        .collect::<Vec<_>>();
    format!(
        "raydock: `--env` goes before the command: `raydock --env {value} {}`",
        rest.join(" ")
    )
}

/// Appends the usage line of the longest known prefix of `command` and where
/// to read its help.
fn with_usage(message: String, command: &[&str]) -> String {
    let known = (0..=command.len())
        .rev()
        .find_map(|len| crate::help_for_command_path(&command[..len]).map(|help| (len, help)));
    let Some((len, help)) = known else {
        return message;
    };
    let usage = help
        .lines()
        .find(|line| line.starts_with("Usage:"))
        .unwrap_or_default();
    let help_command = command[..len]
        .iter()
        .map(|word| format!(" {word}"))
        .collect::<String>();
    format!("{message}\n\n{usage}\nRun `raydock{help_command} --help` for details.")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use crate::{RaylineDispatch, rayline_dispatch_for_argv};

    fn dispatch(args: &[&str]) -> RaylineDispatch {
        let argv = std::iter::once("raydock")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect::<Vec<_>>();
        rayline_dispatch_for_argv(&argv)
    }

    fn usage_message(args: &[&str]) -> String {
        match dispatch(args) {
            RaylineDispatch::Usage(message) => {
                assert!(!message.contains("not available"), "{args:?}: {message}");
                message
            }
            other => panic!("{args:?}: expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command_is_named() {
        let message = usage_message(&["nosuchcmd"]);
        assert!(message.starts_with("raydock: unknown command `nosuchcmd`"));
        assert!(message.contains("Run `raydock --help` for details."));
    }

    #[test]
    fn unknown_root_option_is_named() {
        let message = usage_message(&["--bogus", "top"]);
        assert!(message.starts_with("raydock: unknown option `--bogus`"));
    }

    #[test]
    fn extra_argument_is_named_with_the_command_usage() {
        let message = usage_message(&["top", "extra"]);
        assert!(message.starts_with("raydock: `raydock top` does not accept `extra`"));
        assert!(message.contains("\nUsage: raydock top "));
        assert!(message.ends_with("Run `raydock top --help` for details."));
    }

    #[test]
    fn unknown_subcommand_option_is_named_with_the_subcommand_usage() {
        let message = usage_message(&["router", "start", "--bogus"]);
        assert!(message.starts_with("raydock: `raydock router start` does not accept `--bogus`"));
        assert!(message.contains("\nUsage: raydock router start "));
        assert!(message.ends_with("Run `raydock router start --help` for details."));

        let message = usage_message(&["router", "start", "--route", "bogus"]);
        assert!(message.starts_with("raydock: `raydock router start` does not accept `bogus`"));
    }

    #[test]
    fn env_after_a_command_that_does_not_read_it_shows_where_it_goes() {
        assert_eq!(
            usage_message(&["top", "--env", "dev"]),
            "raydock: `--env` goes before the command: `raydock --env dev top`"
        );
        assert_eq!(
            usage_message(&["router", "status", "--env=dev"]),
            "raydock: `--env` goes before the command: `raydock --env dev router status`"
        );
        assert_eq!(
            usage_message(&["top", "--json", "--env"]),
            "raydock: `--env` goes before the command: `raydock --env <name> top --json`"
        );
    }

    #[test]
    fn invalid_environment_name_is_named() {
        for args in [
            &["--env", "bad/env", "status"][..],
            &["--env=bad/env", "status"][..],
            &["status", "--env", "bad/env"][..],
            &["openclaw", "--env", "bad/env"][..],
            &["codex", "--env", "bad/env"][..],
        ] {
            let message = usage_message(args);
            assert!(
                message.starts_with("raydock: invalid environment name `bad/env`"),
                "{args:?}: {message}"
            );
        }
    }

    #[test]
    fn missing_or_conflicting_arguments_name_the_command() {
        let message = usage_message(&["local", "use"]);
        assert!(
            message
                .starts_with("raydock: missing or conflicting arguments for `raydock local use`")
        );
        assert!(message.contains("\nUsage: raydock local use <number|model-id>"));

        let message = usage_message(&["top", "--hybrid", "--legacy"]);
        assert!(message.starts_with("raydock: missing or conflicting arguments for `raydock top`"));
    }

    #[test]
    fn hybrid_with_root_options_says_where_env_goes() {
        for args in [
            &["--env", "dev", "hybrid", "status"][..],
            &["--auth-token", "t", "hybrid"][..],
        ] {
            assert_eq!(usage_message(args), super::HYBRID_ROOT_OPTIONS);
        }
    }

    #[test]
    fn only_private_commands_are_reported_unavailable() {
        for (args, command) in [
            (&["internal", "status"][..], "internal"),
            (&["--env", "dev", "chat"][..], "chat"),
            (&["claude", "login"][..], "claude login"),
            (
                &["claude", "--model", "x", "hooks", "status"][..],
                "claude hooks",
            ),
        ] {
            assert_eq!(
                dispatch(args),
                RaylineDispatch::Unavailable(command.to_owned()),
                "{args:?}"
            );
        }
    }
}
