# Rayline CLI

Rayline CLI is an open-source command-line tool from Atlas Futures, Inc. It launches
your coding harness on ARC, which picks the model for each request.

A harness runs in one of two modes:

- **Cloud mode**, the default. The harness talks to Rayline cloud directly.
- **Local mode.** The harness talks to a Rayline process on your machine. For
  each request, that process sends the request to one of your own inference
  servers or to Rayline cloud, depending on the model ARC picks.

## Install

Release assets are published on
[GitHub Releases](https://github.com/rayline-ai/raydock/releases).

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/rayline-ai/raydock/main/scripts/install-rayline.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/rayline-ai/raydock/main/scripts/install-rayline.ps1 | iex
```

The installers place `rayline`, `rayline-core` and `rayline-core-LICENSE` in
`~/.rayline/bin`. They need
[`minisign`](https://jedisct1.github.io/minisign/): they verify the release's
`SHA256SUMS.minisig` against the pinned release key, then each file they
download against `SHA256SUMS`.

## Quick start

Sign in, then run a harness in cloud mode:

```bash
rayline auth login
rayline run claude
rayline run codex -- exec "summarize this repo"
```

To use local mode, add a `routing` section to `~/.config/rayline/settings.json`,
along with any inference servers you run yourself (see
[Local mode](docs/local-mode.md)). Then run:

```bash
rayline run claude --local
```

| Command | Does |
|---|---|
| `rayline auth` | Signs in, signs out, shows status, prints a token. |
| `rayline run <HARNESS>` | Launches a coding harness on ARC. |
| `rayline local` | Starts, stops or inspects the local Rayline process. |
| `rayline top` | Shows live request metrics. |
| `rayline update` | Checks for or installs a Rayline update. |

## Supported harnesses

`claude` (Claude Code), `codex` (Codex CLI), `opencode`, `pi`, `omp`
(oh-my-pi), `hermes` (Hermes Agent) and `openclaw` (OpenClaw).

## Documentation

- [Getting started](docs/getting-started.md): install, sign-in, running a
  harness, harness options, updates and files.
- [Local mode](docs/local-mode.md): `settings.json`, the local process, live
  metrics and config errors.
- [Acceptance testing](docs/acceptance-testing.md): one real turn per harness
  in cloud and local mode.
- [Release process](docs/release.md): how releases are built and published.

## Build

```bash
cargo build --workspace --locked
```

## Validate

```bash
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 test --workspace --locked -- --test-threads=1
cargo +1.88.0 clippy --workspace --all-targets --locked -- -D warnings
```

## Disclaimers

### Non-Affiliation

Rayline CLI is an independent, open-source project from Atlas Futures, Inc. It
is not affiliated with, endorsed by, or sponsored by Anthropic PBC. "Claude",
"Claude Code", and "Anthropic" are trademarks of Anthropic PBC, used here
nominatively to describe interoperability.

### User Responsibility

You are responsible for making sure your use of Rayline CLI complies with the
terms of service of every provider whose API you route to. Install and use
Rayline CLI only on machines and accounts you control.

## License and Trademarks

Rayline CLI is licensed under the [Apache License 2.0](LICENSE). The Apache
license does not grant rights to the Rayline name or logos. See
[TRADEMARK.md](TRADEMARK.md).

Copyright 2026 Atlas Futures, Inc.
