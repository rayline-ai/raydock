# Getting started

This guide covers installing Rayline, signing in, running a coding harness on
ARC, and the options each harness has.

```
Usage: rayline [OPTIONS] COMMAND [ARGS]...

Options:
  --version        Show version
  --help           Show this message and exit

Commands:
  auth             Sign in, sign out, show status, print a token
  run              Launch a coding harness on ARC
  local            Start, stop or inspect the local Rayline process
  top              Show live request metrics
  update           Check for or install a Rayline update
```

## Install

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/rayline-ai/raydock/main/scripts/install-rayline.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/rayline-ai/raydock/main/scripts/install-rayline.ps1 | iex
```

The installer places `rayline`, `rayline-core` and `rayline-core-LICENSE` in
`~/.rayline/bin`. It needs
[`minisign`](https://jedisct1.github.io/minisign/): it verifies the release's
`SHA256SUMS.minisig` against the pinned release key, then each file it
downloads against `SHA256SUMS`.

## Sign in

```bash
rayline auth login
```

| Command | Does |
|---|---|
| `rayline auth login` | Signs in to Rayline in the browser. |
| `rayline auth logout` | Revokes the stored session and removes the stored router key. |
| `rayline auth status` | Shows the signed-in account and environment. |
| `rayline auth token` | Prints a valid account bearer token. |

`rayline auth login` has two options:

- `-b`, `--no-browser`: sign in with a device code instead of opening a
  browser.
- `-p`, `--paste`: paste the callback URL yourself. If you also pass
  `--no-browser`, `--paste` wins.

If you launch a harness while signed out, and stdin and stdout are both
terminals, Rayline signs you in first, with the browser flow. Otherwise the
launch fails because you are signed out. A local-mode run that reuses a running
local process doesn't read your credentials, so it runs even when you are
signed out.

Sign-in writes `~/.config/rayline/credentials.json`. The file holds your
session and your router key, and sign-in creates both together. `rayline auth
logout` removes them. It is the only place Rayline reads them from: no env var
or flag overrides it. Treat it as a secret. Without the file you are signed
out. If it is invalid, a command that reads it stops with an error that tells
you to run `rayline auth login`, which replaces it.

## Run a harness

```
rayline run <HARNESS> [--local | --config <file>] [HARNESS ARGS]...
```

| Harness | Runs |
|---|---|
| `claude` | Claude Code |
| `codex` | Codex CLI |
| `opencode` | opencode |
| `pi` | pi |
| `omp` | oh-my-pi |
| `hermes` | Hermes Agent |
| `openclaw` | OpenClaw |

```bash
rayline run claude
rayline run claude -- -p "Explain this repository"
rayline run codex -- exec "summarize this repo"
```

- With neither `--local` nor `--config`, the run is in **cloud mode**. The
  harness talks to Rayline cloud directly, and no local process runs. Config
  comes from `~/.config/rayline/settings.json`, and that file is optional.
- `--local` runs in **local mode**, through the local Rayline process. See
  [Local mode](local-mode.md).
- `--config <file>` uses `<file>` instead of `settings.json`, without merging
  the two. The run is in local mode if `<file>` has a `routing` section, and
  in cloud mode if it doesn't.
- `--local` and `--config` together are a usage error.
- Local mode refuses `rayline run claude --isolated`,
  `rayline run codex --model`, `rayline run codex app` and
  `rayline run codex configure`. The error says the option isn't available in
  local mode.
- Local mode also refuses to resume or fork a saved session, before anything
  starts. It checks the harness arguments before any `--`:
  - `claude`: `-r`, `-c`, `--resume`, `--continue`, `--fork-session`,
    `--resume-session-at`;
  - `codex`: `resume` or `fork` as the first argument, or `exec resume` as the
    first two;
  - `pi`: `-r`, `-c`, `--resume`, `--continue`, `--fork`, `--session`,
    `--session-id`;
  - `opencode`: `-c`, `-s`, `--continue`, `--session`, `--fork`.

  Other harnesses' arguments are passed on unchecked.
- Local mode refuses OpenClaw container mode: `rayline run openclaw` with
  `OPENCLAW_CONTAINER` set to a non-empty value, or with a `--container` or
  `--container=<value>` argument.
- Rayline options come right after the harness name. The first argument that
  isn't a Rayline option, and everything after `--`, goes to the harness
  unchanged, including `--help`.
- `rayline run --help` prints the usage. An unknown harness is a usage error.

Rayline writes the files it generates for a harness under
`~/.rayline/harness/<name>/` in cloud mode and under
`~/.rayline/local/harness/<name>/` in local mode. Each mode writes its own
endpoint, key and model list there.

### Claude Code

`--isolated` gives Claude Code its own config folder,
`~/.rayline/harness/claude-code/`, so it can run beside your other Claude Code
sessions:

```bash
rayline run claude --isolated
```

### Codex

| Option or subcommand | Does |
|---|---|
| `--model <model>` | The model Codex asks for. |
| `rayline run codex app [WORKSPACE]` | Launches the Codex desktop app on its own `CODEX_HOME`, `~/.rayline/harness/codex-app/`, and opens `WORKSPACE` if you give one. |
| `rayline run codex configure` | Writes a Codex profile, `rayline.config.toml`, to your Codex home: `CODEX_HOME`, or `~/.codex` if it isn't set. |

### opencode, pi, oh-my-pi, Hermes Agent and OpenClaw

These take only the options every harness has, `--local` and `--config`. Every
other argument goes to the harness:

```bash
rayline run omp
rayline run pi -- "Fix the failing test in parser.py"
```

## Local mode

Local mode routes each request through a Rayline process on your machine, so
ARC can send it to your own inference servers. See [Local mode](local-mode.md).

## Watch requests

`rayline top` shows live request metrics from the local process, and
`rayline top --json` prints one snapshot. Both need a running local process.

## Update

```bash
rayline update
rayline update --check
```

| Option | Does |
|---|---|
| `--check` | Reports whether an update is available, without installing it. |
| `--version <version>` | Installs this version. |
| `--force` | Installs even when the target version isn't newer. |
| `--dry-run` | Downloads and verifies the update, then reports what it would install, without installing it. |

If Rayline was installed with Homebrew or comes with Rayline.app,
`rayline update` installs nothing and tells you to update through Homebrew or
the app instead.

## Files

`~/.config/rayline/` holds only two files:

- `settings.json`, which you write. It is optional.
- `credentials.json`, which `rayline auth login` and `rayline auth logout`
  write.

`~/.rayline/` holds the files Rayline writes:

```
~/.rayline/
  bin/                          rayline, rayline-core, rayline-core-LICENSE
  local/                        local process state
    harness/<name>/             files Rayline generates per harness in local mode
  harness/<name>/               files Rayline generates per harness in cloud mode
```

## Config errors

Rayline checks `settings.json`, env overrides and flags once, when it loads,
before any command runs. A command that uses `credentials.json` checks it
before it acts. If something is wrong it refuses to run, with an error that
names the key or flag. [Local mode](local-mode.md#config-errors) lists
every config check.
