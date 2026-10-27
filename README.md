# Raydock

Raydock is an open-source router from Atlas Futures, Inc. that runs on your
machine and sits between your coding agent and the AI models it talks to,
deciding where each request should go. The point is hybrid sessions: keep your
main agent on a frontier cloud model while quietly sending cheaper, high-volume
work — like background subagent tasks — to a fast model running locally.

It ships the `raydock` CLI, the `rld` daemon, and the `rayline-core` execution core. The `rayline` command is a supported alias for `raydock`. Using it
locally through `raydock claude --local` needs no account and never connects to
Rayline's hosted service — routing decisions are made on your machine, and any
cloud model traffic uses your own credentials.

The explicit `raydock hybrid` mode connects an unmodified coding harness to ARC
decisions and the standalone execution core, with your own local model server
(llama.cpp, Ollama or MLX) and named cloud targets. Easy work stays on device;
harder work, and context that outgrows the local window, goes to the cloud.
Start with [hybrid setup](docs/hybrid-local-setup.md) (sign in, point at your
local model, start); see [ARC hybrid execution](docs/hybrid-routing.md) for
configuration, lifecycle, and validation.

Raydock was previously called Rayline Local. Use `raydock` for new commands;
`rayline` remains a supported alias. Configuration, credentials, environment
variables, and update channels keep their existing paths and names.

To migrate an installation at 0.2.14, run `rayline update`, then run
`rayline --version` once to finish creating the new command and alias. The
migration notice goes to stderr. Homebrew users should run `brew update` and
`brew upgrade raydock`; the tap migrates the old `rayline` formula automatically.

## Demo

Claude Code running with hybrid cloud + on-device AI. The main agent runs Opus
in the cloud and orchestrates an `Explore` subagent that Raydock routes to
a model running fully on your machine (Qwen3.6-35B-A3B, Q4) — seamlessly, in a
single session.

<a href="https://get.rayline.ai/media/rayline-local-routing-demo.mp4">
  <img src="https://get.rayline.ai/media/rayline-local-routing-demo-4x.gif"
       alt="Raydock routing a Claude Code Explore subagent to an on-device model"
       width="100%">
</a>

## Install

Release assets are published on
[GitHub Releases](https://github.com/rayline-ai/rayline/releases).

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/rayline-ai/rayline/main/scripts/install-rayline.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/rayline-ai/rayline/main/scripts/install-rayline.ps1 | iex
```

The installers place `raydock` and `rld` in `~/.rayline/bin` by default and
verify every downloaded binary against the release `SHA256SUMS` file.

## Quick Start

Start a Claude Code session with the auth-free local router. Your conversation
works exactly as it normally does — Rayline just routes background subagent work
to your on-device model:

```bash
raydock claude --local
```

Run it alongside a normal Claude Code session, in its own isolated config dir:

```bash
raydock claude --local --isolated
```

Check on the router, follow its logs, or stop it:

```bash
raydock router status
raydock router logs --lines 120
raydock router stop
```

Run Codex CLI through Rayline's local OpenAI Responses-compatible router. With
no `--config`, Rayline reuses Codex's existing ChatGPT/Codex subscription login
and forwards through the ChatGPT Codex backend:

```bash
raydock codex -- exec "summarize this repo"
```

Launch the Codex **desktop app** through Rayline in one step — it accepts the
same flags as `raydock codex`, starts the router, and points the app at Rayline:

```bash
raydock codex app                 # opens the desktop app routed through Rayline
raydock codex app ~/projects/app  # optionally open a workspace
```

Rayline sets the app up on an isolated Codex home so it doesn't disturb your
normal Codex configuration. Because the desktop app is single-instance, if it is
already running with a different configuration, `raydock codex app` prompts
before restarting it.

Use Claude Code inside the **Claude desktop app** on ARC. Desktop's Code sessions
only reach a gateway through the app's third-party inference mode, so
`raydock claude-desktop` runs a dedicated local router (its own ports and state,
separate from `raydock claude`), saves and selects a **Rayline ARC** inference
profile, and opens Desktop. If configuration changed, a running Desktop is
first asked to quit gracefully:

```bash
raydock claude-desktop                  # configure and open Desktop on ARC
raydock claude-desktop --manual         # router only; print manual import steps
raydock claude-desktop validate         # check Desktop's applied configuration
raydock claude-desktop install-service  # keep the router running across logins (macOS)
raydock claude-desktop off              # restore the previous profile and mode
```

Third-party mode applies to the whole Desktop app and switches away from your
claude.ai subscription while it is active. Your Rayline key stays in the router.
Desktop gets a random per-instance token that only unlocks this loopback router,
which also refuses non-loopback Host/Origin headers and non-JSON bodies.
The previous profile and mode are recorded before switching; `off` restores
those selections while preserving other Desktop preferences. Local automation
is unavailable when organization policy controls the configuration. `--manual`
retains the import workflow for that case and for Desktop compatibility issues.

Check for CLI updates:

```bash
raydock update --check
```

Run `raydock --help` for the full list of commands. For setup, configuration,
and provider endpoints, see the [Getting Started guide](docs/getting-started.md).

## How Routing Works

Three small flags on `raydock claude` decide where requests go. **Most people
only ever need `--local`** — the other two are advanced overrides.

| Flag | Question it answers | Values | Default |
| --- | --- | --- | --- |
| `--local` | Who decides routing? | on-device router when present, hosted cloud router when absent | cloud |
| `--via` | How does Claude Code connect? | `proxy`, `env` | `proxy` |
| `--route` | What flows through the router? | `all`, `subagents` | depends on router |

- `--local` runs the on-device static router: no login, and routing decisions
  never leave your machine. Without it, the hosted cloud router at
  `api.rayline.ai` makes the decisions (needs `raydock auth login`).
- By default, local sessions are **hybrid**: your main agent stays on cloud
  Claude and only subagent traffic goes through the router. Pass `--route all`
  to send the main agent through the router too. Where it lands is then up to
  the router config: the built-in default keeps main on cloud Claude, so a
  fully-local session also needs a config with `routes.main.endpoint: local`
  (see `--router-config-path`).

The [Getting Started guide](docs/getting-started.md#choosing-where-requests-go)
has the full matrix and every valid combination.

## Use Rayline From Code or Agents

You can also send your own Anthropic API traffic through Rayline — from a script
or your own agent — using the official Anthropic SDKs. Examples come in Python
and TypeScript, grouped by routing path:

- **Cloud router** — point the SDK at `https://api.rayline.ai` with a router key:
  [examples/cloud/python](examples/cloud/python) ·
  [examples/cloud/typescript](examples/cloud/typescript)
- **Local routing** — start the router with `raydock router start`, then send the
  SDK through the proxy on `127.0.0.1:20810` and request model `rayline-local` so
  the call lands on your on-device model:
  [examples/local/python](examples/local/python) ·
  [examples/local/typescript](examples/local/typescript)
- **Codex / OpenAI Responses** — start the router with
  `raydock router start --mode codex` and point Codex at
  `http://127.0.0.1:20811/v1`, run `raydock codex ...` to have Rayline pass the
  provider overrides automatically, or `raydock codex app` to launch the Codex
  desktop app routed through Rayline. The default no-config path reuses
  Codex's ChatGPT subscription auth; explicit configs can route selected
  requests to local/API-key endpoints or to the hosted **cloud router (RCR)** at
  `api.rayline.ai` (the `R*` modes), which serves Codex natively over
  `/v1/responses` and picks a GPT model. Rayline supports Codex's Responses create
  stream, model catalog, compaction, memory-summary, images, and search provider
  calls, with native passthrough when the selected route is an `openai_responses`
  endpoint.

## Supported Clients

- Claude Code, Anthropic's CLI coding agent.
- Codex CLI and Codex app via OpenAI Responses-compatible local routing.

More clients may be supported over time.

## Build

```bash
cargo build --workspace --locked
cargo build --release -p rayline-cli -p rayline-daemon --locked
```

## Validate

```bash
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 test --workspace --locked -- --test-threads=1
cargo +1.88.0 clippy --workspace --all-targets --locked -- -D warnings
```

## Documentation

- [Getting Started](docs/getting-started.md) — setup, routing flags, provider
  endpoints, logs, and troubleshooting.
- [Acceptance testing](docs/acceptance-testing.md) — validating the end-to-end
  Claude Code path through Raydock.
- [Release process](docs/release.md) — how releases are built and published.

## Disclaimers

### Non-Affiliation

Raydock is an independent, open-source project from Atlas Futures, Inc. It
is not affiliated with, endorsed by, or sponsored by Anthropic PBC. "Claude",
"Claude Code", and "Anthropic" are trademarks of Anthropic PBC, used here
nominatively to describe interoperability.

### User Responsibility and Local TLS Interception

To route traffic in proxy modes, Raydock can install a local certificate
authority on your machine and intercept TLS traffic to provider APIs locally,
using your own credentials. You are responsible for ensuring your use of
Raydock complies with the terms of service of any provider whose API you
route to. Install and use Raydock only on machines and accounts you
control.

## License and Trademarks

Raydock is licensed under the [Apache License 2.0](LICENSE). The Apache
license does not grant rights to the Raydock name or logos. See
[TRADEMARK.md](TRADEMARK.md).

Copyright 2026 Atlas Futures, Inc.
