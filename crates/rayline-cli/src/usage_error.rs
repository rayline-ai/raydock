//! Error messages for rejected command lines.
//!
//! The argument parsers return `None` on any rejection. [`TrackedArgs`]
//! records how far they read, so the message can name the argument parsing
//! stopped at and show the usage line of the command it belongs to.

use std::cell::Cell;
use std::ffi::OsString;

use crate::RaylineDispatch;

/// Commands that exist only in non-public builds; only these report that they
/// are not available here. Release packaging checks rely on this message for
/// these commands, so keep them listed. The public parser has no other record of them; the
/// `claude` management words are `is_claude_management_subcommand`.
const PRIVATE_COMMANDS: &[&str] = &[
    "internal",
    "install-cli",
    "hook-send",
    "agent",
    "chat",
    "api",
    "menubar",
];

pub(crate) const HYBRID_ROOT_OPTIONS: &str = "\
raydock: `hybrid` reads `--env` after its subcommand (`raydock hybrid start --env dev`) \
and has no `--auth-token`
Run `raydock hybrid --help` for details.";

/// The arguments after the program name, counting how many a parser has
/// taken and whether it asked for more than there were.
///
/// The count only locates a rejection if parsers pull arguments one at a time
/// and reject at the last one pulled (a `peek` counts as pulled). A parser must
/// not collect the remaining arguments before it has finished validating them.
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
        let last = args.last().copied().unwrap_or_default();
        let message = if last == "--env" {
            let rest = if command.is_empty() {
                String::new()
            } else {
                format!(" {}", command.join(" "))
            };
            format!("raydock: `--env` needs a value: `raydock --env <name>{rest}`")
        } else if crate::is_value_option(last) && !last.contains('=') {
            format!("raydock: `{last}` needs a value")
        } else if command.is_empty() {
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
    // Never echo a token.
    if stop > 0 && args[stop - 1] == "--auth-token" {
        let message = "raydock: invalid `--auth-token` value".to_owned();
        return RaylineDispatch::Usage(with_usage(message, &command));
    }

    if let Some(env) = env_value_at(&args, stop) {
        if !crate::status::is_valid_root_env(env) {
            // Not echoed: a mistyped command line can put a key here.
            let message =
                "raydock: invalid `--env` name (use letters, digits, `-` and `_`)".to_owned();
            return RaylineDispatch::Usage(with_usage(message, &command));
        }
    }
    if command.is_empty() {
        if PRIVATE_COMMANDS.contains(&bad) {
            return RaylineDispatch::Unavailable(bad.to_owned());
        }
        let message = if bad.starts_with('-') {
            match option_name(bad) {
                Some(name) => format!("raydock: unknown option `{name}`"),
                None => "raydock: unknown option".to_owned(),
            }
        } else {
            // Echoed only if it looks like a command: it may be a prompt or a key.
            if is_word(bad) {
                format!("raydock: unknown command `{bad}`")
            } else {
                "raydock: unknown command".to_owned()
            }
        };
        return RaylineDispatch::Usage(with_usage(message, &command));
    }
    // A management word as the value of a claude option (`--via login`) is a
    // bad value, not a management subcommand.
    let previous = args[stop - 1];
    let is_claude_value = crate::is_value_option(previous) && !previous.contains('=');
    if command[0] == "claude" && !is_claude_value && crate::is_claude_management_subcommand(bad) {
        return RaylineDispatch::Unavailable(format!("claude {bad}"));
    }
    // Only option names are echoed: a rejected word may be a prompt or a key.
    let words = command.join(" ");
    let previous_is_value =
        stop >= 2 && crate::is_value_option(args[stop - 2]) && !args[stop - 2].contains('=');
    let previous_option =
        option_name(previous).filter(|name| *name == previous && !previous_is_value);
    // "Rejected" rather than "unknown": a known option can be rejected as the
    // value of the option before it, or for its own value.
    let after = previous_option.map_or(String::new(), |option| format!(" after `{option}`"));
    // Only these commands have no `--env` of their own; elsewhere a rejected
    // `--env` was read as another option's value (`codex --auth --env dev`).
    let without_env = matches!(command[0], "top" | "router" | "local" | "update");
    if without_env && (bad == "--env" || bad.starts_with("--env=")) {
        // After another option `--env` may have been read as its value: say so too.
        let hint = misplaced_env(&command);
        return RaylineDispatch::Usage(if after.is_empty() {
            hint
        } else {
            format!("raydock: `raydock {words}` rejected `--env`{after}\n{hint}")
        });
    }
    let message = match (bad.starts_with('-'), option_name(bad)) {
        (true, Some(name)) if bad.contains('=') && after.is_empty() => {
            format!("raydock: `raydock {words}` rejected the value of `{name}`")
        }
        (true, Some(name)) => format!("raydock: `raydock {words}` rejected `{name}`{after}"),
        (true, None) => format!("raydock: `raydock {words}` rejected an option{after}"),
        (false, _) if previous_option.is_some() => {
            format!("raydock: `raydock {words}` rejected the argument{after}")
        }
        (false, _) => format!("raydock: `raydock {words}` got an unexpected argument"),
    };
    RaylineDispatch::Usage(with_usage(message, &command))
}

/// Whether `text` looks like a command or option word: lowercase letters,
/// digits and `-`, and short (every Raydock word is under 32 characters).
/// Anything else may be a prompt or a key and is not echoed.
fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The name of the option `token` if it looks like one: a `--word` without
/// any `=value`, or a short `-x` without an attached value (`-n40`).
fn option_name(token: &str) -> Option<&str> {
    let name = token.split_once('=').map_or(token, |(name, _)| name);
    match name.strip_prefix("--") {
        Some(long) => is_word(long).then_some(name),
        None => token
            .get(..2)
            .filter(|short| short.starts_with('-') && short.as_bytes()[1].is_ascii_alphabetic()),
    }
}

/// The command words in `args[..end]`: the first word after the root options
/// and the known subcommand words after it. Other words (such as a prompt
/// passed through to a harness) are left out, so they are never echoed.
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
    let known = (1..=command.len())
        .rev()
        .find(|len| crate::help_for_command_path(&command[..*len]).is_some())
        .unwrap_or(command.len().min(1));
    command.truncate(known);
    command
}

/// The `--env` value at `index`, as `--env=VALUE` or as the word after `--env`.
/// An option after `--env` is not its value.
fn env_value_at<'a>(args: &[&'a str], index: usize) -> Option<&'a str> {
    args[index].strip_prefix("--env=").or_else(|| {
        (index > 0 && args[index - 1] == "--env" && !args[index].starts_with('-'))
            .then_some(args[index])
    })
}

/// `--env` after a command that does not read it: show the command with
/// `--env` moved in front, where every command but `hybrid` reads it (`hybrid`
/// never reaches here: its arguments are not parsed by this parser). Only the command words
/// are repeated; the value is a placeholder, since a mistyped line can put a
/// prompt or a key there.
fn misplaced_env(command: &[&str]) -> String {
    format!(
        "raydock: `--env` goes before the command: `raydock --env <name> {}`",
        command.join(" ")
    )
}

/// Appends the usage line of the longest known prefix of `command` and where
/// to read its help.
fn with_usage(message: String, command: &[&str]) -> String {
    // A launcher (`raydock openclaw`) has no help of its own: show the root's.
    let (command, help) = match crate::help_for_command_path(command) {
        Some(help) => (command, help),
        None => (&[][..], crate::ROOT_HELP),
    };
    let usage = help
        .lines()
        .find(|line| line.starts_with("Usage:"))
        .unwrap_or_default();
    let help_command = command
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
        assert!(message.starts_with("raydock: `raydock top` got an unexpected argument"));
        assert!(message.contains("\nUsage: raydock top "));
        assert!(message.ends_with("Run `raydock top --help` for details."));
    }

    #[test]
    fn unknown_subcommand_option_is_named_with_the_subcommand_usage() {
        let message = usage_message(&["router", "start", "--bogus"]);
        assert!(message.starts_with("raydock: `raydock router start` rejected `--bogus`"));
        assert!(message.contains("\nUsage: raydock router start "));
        assert!(message.ends_with("Run `raydock router start --help` for details."));

        let message = usage_message(&["router", "start", "--route", "bogus"]);
        assert!(
            message.starts_with(
                "raydock: `raydock router start` rejected the argument after `--route`"
            )
        );
    }

    #[test]
    fn env_after_a_command_that_does_not_read_it_shows_where_it_goes() {
        assert_eq!(
            usage_message(&["top", "--env", "dev"]),
            "raydock: `--env` goes before the command: `raydock --env <name> top`"
        );
        assert_eq!(
            usage_message(&["router", "status", "--env=dev"]),
            "raydock: `--env` goes before the command: `raydock --env <name> router status`"
        );
        assert_eq!(
            usage_message(&["top", "--json", "--env"]),
            "raydock: `raydock top` rejected `--env` after `--json`\n\
             raydock: `--env` goes before the command: `raydock --env <name> top`"
        );
        assert!(
            usage_message(&["router", "start", "--route", "--env", "dev"])
                .starts_with("raydock: `raydock router start` rejected `--env` after `--route`\n")
        );
        let message = usage_message(&["--auth-token", "secret-value", "top", "--env", "dev"]);
        assert!(!message.contains("secret-value"), "{message}");
        for args in [
            &["top", "--auth-token=secret-value"][..],
            &["top", "--api-key=secret-value"][..],
            &["--api-key=secret-value", "top"][..],
        ] {
            let message = usage_message(args);
            assert!(!message.contains("secret-value"), "{args:?}: {message}");
        }
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
                message.starts_with("raydock: invalid `--env` name")
                    && !message.contains("bad/env"),
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
    fn option_without_its_value_is_named() {
        let message = usage_message(&["openclaw", "--env"]);
        assert!(
            message.starts_with("raydock: `--env` needs a value: `raydock --env <name> openclaw`"),
            "{message}"
        );
        let message = usage_message(&["--env"]);
        assert!(message.starts_with("raydock: `--env` needs a value: `raydock --env <name>`"));
        let message = usage_message(&["status", "--auth-token"]);
        assert!(message.starts_with("raydock: `--auth-token` needs a value"));
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
    fn messages_do_not_echo_passthrough_words_or_tokens() {
        for args in [
            &["claude", "private prompt", "--via", "bogus"][..],
            &["claude", "private prompt", "--local", "--via", "env"][..],
            &["top", "--env", "--auth-token=secret-value"][..],
            &["--env", "--auth-token=secret-value", "status"][..],
            &["--env=--auth-token=secret-value", "status"][..],
            &["top", "--env", "secret-value"][..],
            &["claude", "--via", "secret-value"][..],
            &["codex", "--auth", "secret-value"][..],
            &["router", "logs", "-n", "secret-value"][..],
            &["top", "secret-value"][..],
            &["router", "logs", "-nsecret-value"][..],
            &["status", "--auth-token", "-secret-value", "unexpected"][..],
            &["status", "--auth-token", "--secret-value", "unexpected"][..],
            &["codex", "--env", "--private prompt"][..],
            &["top", "--Secret-Value"][..],
            &["private prompt"][..],
        ] {
            let message = usage_message(args);
            assert!(
                !message.contains("private prompt") && !message.contains("secret-value"),
                "{args:?}: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_auth_token_is_not_echoed() {
        use std::os::unix::ffi::OsStringExt;

        for prefix in [&["status"][..], &[][..]] {
            let mut argv = std::iter::once("raydock")
                .chain(prefix.iter().copied())
                .chain(["--auth-token"])
                .map(OsString::from)
                .collect::<Vec<_>>();
            argv.push(OsString::from_vec(b"secret-value\xff".to_vec()));
            argv.push(OsString::from("status"));
            match rayline_dispatch_for_argv(&argv) {
                RaylineDispatch::Usage(message) => {
                    assert!(!message.contains("secret-value"), "{message}");
                    assert!(message.starts_with("raydock: invalid `--auth-token` value"));
                }
                other => panic!("expected a usage error, got {other:?}"),
            }
        }
    }

    #[test]
    fn known_option_rejected_as_a_value_is_named_with_its_owner() {
        let message = usage_message(&["codex", "--auth", "--subscription"]);
        assert!(
            message
                .starts_with("raydock: `raydock codex` rejected `--subscription` after `--auth`")
        );
        // A private-build flag is an ordinary rejected option here.
        let message = usage_message(&["claude", "--no-telemetry"]);
        assert!(message.starts_with("raydock: `raydock claude` rejected `--no-telemetry`"));
        let message = usage_message(&["codex", "--auth", "--env", "dev"]);
        assert!(message.starts_with("raydock: `raydock codex` rejected `--env` after `--auth`"));
        let message = usage_message(&["codex", "--auth", "--env=dev"]);
        assert!(message.starts_with("raydock: `raydock codex` rejected `--env` after `--auth`"));
        let message = usage_message(&["claude", "--via=bogus"]);
        assert!(message.starts_with("raydock: `raydock claude` rejected the value of `--via`"));
    }

    #[test]
    fn management_word_as_an_option_value_is_a_usage_error() {
        let message = usage_message(&["claude", "--via", "login"]);
        assert!(
            message.starts_with("raydock: `raydock claude` rejected the argument after `--via`")
        );
    }

    #[test]
    fn only_private_commands_are_reported_unavailable() {
        for (args, command) in [
            (&["internal", "status"][..], "internal"),
            (&["--env", "dev", "chat"][..], "chat"),
            (&["claude", "login"][..], "claude login"),
            (&["api"][..], "api"),
            (&["menubar"][..], "menubar"),
            (&["claude", "--local", "login"][..], "claude login"),
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
