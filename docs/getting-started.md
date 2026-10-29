# Getting Started with Raydock

This guide takes you from a fresh install to a working hybrid session — your
main agent on cloud Claude, background subagents on a model running locally — and
then into the configuration you'll reach for as you go deeper.

Two ways to run it:

- **`raydock claude --local`** — the local static router. No hosted account, no
  network calls for routing decisions. Out of the box it sends read-only
  subagents such as Explore to your local model and keeps everything else,
  including the main agent, on cloud Claude.
- **`raydock claude`** — adds hosted Rayline routing on top of Claude Code. This
  signs you in with a Rayline-scoped CLI session (an opaque `rls_`/`rlr_` token)
  and mints a separate `rlk-` router key for hosted requests.

## Quick Start

Start a Claude Code session with the local router:

```bash
raydock claude --local
```

Run it next to a normal Claude Code background daemon, using an isolated Claude
config dir so the two don't interfere:

```bash
raydock claude --local --isolated
```

You can also start the router **without** launching Claude Code — handy when you
want to drive it from your own code (for example an Anthropic SDK client pointed
at the proxy on `http://127.0.0.1:20810`). It routes every request through the
router by default, so requesting model `rayline-local` reaches your on-device
model:

```bash
raydock router start                 # route all (default)
raydock router start --route subagents
raydock router stop                  # stop it when done
```

See [examples/cloud](../examples/cloud) and [examples/local](../examples/local)
for runnable Python and TypeScript clients.

## Codex CLI And App

Rayline can also expose a local OpenAI Responses-compatible endpoint for Codex.
This path does not use the Anthropic transparent proxy or a local CA; Codex talks
directly to Rayline over HTTP on `127.0.0.1`.

For a one-off Codex CLI run, let Rayline start the router and pass temporary
Codex provider overrides:

```bash
raydock codex -- exec "summarize this repo"
```

With no `--config`, `raydock codex` defaults to **Rc-Rc** — it routes everything
(main + subagents) to the hosted cloud router (RCR) over native OpenAI Responses,
authenticated with your `raydock auth login` session key, on ARC: the main agent
on `arc-sage-1.0` and subagents on `arc-swift-1.0`. This mirrors the default
`raydock claude`, which likewise routes everything to the cloud router on ARC; a
`--config` overrides selected routes to local/API-key/subscription endpoints. For
the Legacy Rules Router, pass `--model rayline-router` (with a hosted `routes.main`,
Raydock pins it to Legacy when it prepares the config for Codex), or set the route models in the generated
`~/.config/rayline/router.json` to `rayline-router`.

To use your ChatGPT/Codex subscription for the main agent instead, pass
`--auth subscription` (Codex reuses its existing ChatGPT login; subagents route
to the on-device model when one is available). Use `--auth none` for local/no
hosted auth.

Use `--model <name>` before `--` to request a different Rayline or configured
model. Without it the session runs on ARC (see [Which model runs](#which-model-runs-arc-by-default));
`--model rayline-router` selects the Legacy Rules Router, and naming a concrete
model instead **pins** it, so the router's choice is skipped. Two lanes ignore an inherited `claude-*` id by
design — background haiku calls and Claude Code subagent turns. See
[routing-modes](../examples/routing-modes/README.md#what-model-means-at-the-rayline-cloud-endpoint).

### Desktop app

`raydock codex app` launches the Codex **desktop app** through Rayline in one
step. It accepts the same flags as `raydock codex` (`--config`, `--auth`,
`--model`), starts the router, and points the app at Rayline:

```bash
raydock codex app                 # launch the desktop app routed through Rayline
raydock codex app ~/projects/app  # optionally open a workspace path
raydock codex app --auth none --config <path>   # local/API-key routing
```

Rayline runs the desktop app on an isolated Codex home
(`~/.rayline/codex-app-home`) so it never disturbs your normal Codex
configuration:

- On first launch it seeds that home from your existing Codex config (preserving
  your settings) and uses your Codex login when auth resolves to subscription.
  On later launches it refreshes only the Rayline routing and preserves whatever
  the app has since saved (open projects, plugins, and other app settings).
- The app is single-instance. If it is already running with a **different**
  configuration, `raydock codex app` prompts before quitting and relaunching it;
  if it already matches, it just brings the window to the front.
- With `--auth none` (or a local-only config), your Codex login is not used for
  the isolated app.

### Reusable CLI profile

To write a standalone Codex profile (for the CLI, or manual configuration)
instead of launching the app, use `raydock codex configure` and start the
router yourself:

```bash
raydock codex configure
raydock router start --mode codex --auth subscription
```

`raydock codex configure` writes `$CODEX_HOME/rayline.config.toml` when
`CODEX_HOME` is set, otherwise `~/.codex/rayline.config.toml`. The generated
profile points Codex at:

```text
http://127.0.0.1:20811/v1
```

Equivalent manual Codex config:

```toml
model = "rayline-local"
model_provider = "rayline"
forced_login_method = "chatgpt"

[model_providers.rayline]
name = "Raydock"
base_url = "http://127.0.0.1:20811/v1"
wire_api = "responses"
requires_openai_auth = true
```

The router command also accepts `raydock router --mode openai` as shorthand for
`raydock router start --mode codex`.

Check for CLI updates:

```bash
raydock update --check
```

Inspect or stop the router and proxy processes:

```bash
raydock router status
raydock router logs
raydock router logs --lines 120
raydock router top
raydock router stop
```

## Claude Desktop (Code)

`raydock claude-desktop` connects Claude Code inside the Claude desktop app to ARC.
Desktop's Code sessions ignore `ANTHROPIC_BASE_URL`; they reach a gateway only
through the app's third-party inference ("3P") mode, and Desktop accepts only
model ids that look like Anthropic models. The launcher therefore:

- runs a **dedicated** `rld` with its own state (`~/.rayline/claude-desktop`)
  and ports (router 20831, adapter 20828, injector 20829, metrics 20833), so it
  never touches the shared router on 20808–20814 or your Claude Code CLI config;
- offers Desktop two models, shown in the picker as **arc-sage** (the default)
  and **arc-swift** (the cheaper tier). Desktop requires Claude-style model
  ids, so the import uses `claude-arc-sage` and `claude-arc-swift` internally,
  and explicit `model_routes` entries map them to the first-party tier ids
  `arc-sage` and `arc-swift` (`--prefixed-ids` forwards the `rayline/arc-*`
  aliases instead, for a router that predates the bare ids), stamping `x-rayline-client: claude-desktop`. ARC is not
  a Claude model, and Rayline is not affiliated with Anthropic;
- keeps your `rlk-` key in the router's environment only. Desktop gets
  `http://127.0.0.1:20831` and a random per-instance gateway token
  (`~/.rayline/claude-desktop/gateway-token`, mode 600) through
  `~/.rayline/claude-desktop/claude-desktop-import.json`. By default the launcher
  installs the same configuration as a named **Rayline ARC** profile in
  `~/Library/Application Support/Claude-3p/configLibrary/`, selects it and
  opens Desktop. A running Desktop is asked to quit gracefully before changes;
  if it refuses, no selection changes are made. `--manual` instead prints
  **Developer → Configure Third-Party Inference… → Import configuration**
  instructions (skip enabling Developer Mode if the Developer menu exists).
  The router answers only requests that carry
  that token, address a loopback Host on its port, carry no non-loopback
  Origin, and send JSON, so other local processes and web pages (DNS
  rebinding, cross-site `text/plain` posts) cannot spend on your account.
  After every start or restart the launcher checks that the router refuses a
  request without the token. If it does not (for example an `rld` older than
  the CLI), the launcher stops it and exits with an error;
- routes only to ARC: the router config replaces the built-in endpoints, so
  no model id can reach another provider, and it restarts the router when the
  key, token, config or `rld` changes. The login service pins the `rld` path
  the launcher resolved. Claude Desktop's third-party mode is macOS-only, so
  the launcher targets macOS (the router also runs on Linux, for testing);

```bash
raydock claude-desktop                    # configure and launch Desktop
raydock claude-desktop --manual           # router only; print import instructions
raydock --env dev claude-desktop          # route to the dev environment instead
raydock claude-desktop status             # health, request totals, Desktop check
raydock claude-desktop validate           # Desktop's applied config vs. the router
raydock claude-desktop install-service    # LaunchAgent ai.rayline.claude-desktop
raydock claude-desktop stop
raydock claude-desktop off                # switch Desktop back to your subscription
raydock claude-desktop uninstall          # restore selection, remove service + router state
```

3P mode applies to the whole Desktop app: it switches away from your
claude.ai subscription, and Chat and Cowork are off while it is active. To
switch back after automatic setup, run `raydock claude-desktop off`: it restores
both the previously selected profile and deployment mode, then reopens Desktop.
You may need to sign in again. `stop` only stops the router. `uninstall` restores
before deleting the recovery record; the saved Rayline profile stays available.
Restoration and gateway cleanup also work if Claude.app was moved or removed;
if Desktop cannot be reopened, the command reports that without undoing recovery.
For manual imports, `off` prints the steps to sign out of Gateway mode and sign
in with your Anthropic account.

Recovery state is owner-only in `~/.rayline/claude-desktop/desktop-profile.json`.
Repeated starts reuse the same profile and preserve the original recovery
selection. Changes to unrelated preferences and saved profiles are preserved.
The **Rayline ARC** profile itself is managed by Rayline and rewritten on each
start (for example after a gateway token change); save edits under another
profile name. Automatic setup refuses externally changed profile selections,
hybrid pointers, or deployment modes, unreadable previous profiles, unknown
profile metadata, and organization-managed inference settings instead of
overwriting them. Interrupted writes retain recovery state for `off`. Desktop
configuration and recovery writes never follow symlinks and are synced to disk
in order. Do not delete
that file while recovery is pending. A failed launch attempts to restore the
previous selection; the gateway remains running for manual setup. Lifecycle
commands share a nonblocking cross-process lease at `~/.rayline/claude-desktop.lock`
so another start cannot interleave with restoration or uninstall. That small
lock file deliberately survives uninstall. Manual-only cleanup does not inspect
Desktop paths when no recovery is pending.

The library location and restart behavior are documented in
[Anthropic's configuration guide](https://claude.com/docs/third-party/claude-desktop/configuration-format).
The exact metadata and deployment-mode writes are a compatibility boundary,
verified against Desktop 3.6556.2; they are not a public profile-launch API.
Anthropic does not support routing
Claude Code to non-Claude models through a gateway, so a Desktop update can
change this behaviour; `scripts/smoke-claude-app.sh` checks the wire path with
one headless Claude Code turn per model.

## Which Model Runs (ARC by Default)

Hosted launches run on ARC unless you choose otherwise. The main agent uses
`arc-sage-1.0`; subagents and background tasks use `arc-swift-1.0` wherever the
harness gives them their own model setting:

| Launcher | Main agent | Subagents / background | Legacy Rules Router |
|---|---|---|---|
| `raydock claude` (hosted) | `arc-sage-1.0` | `arc-swift-1.0` (`CLAUDE_CODE_SUBAGENT_MODEL`, and `ANTHROPIC_DEFAULT_HAIKU_MODEL` with `--route all`) | `--model rayline-router`; with `--route subagents`, `CLAUDE_CODE_SUBAGENT_MODEL=rayline-router` |
| `raydock codex` (default Rc-Rc) | `arc-sage-1.0` | `arc-swift-1.0` | `--model rayline-router`, or set the route models in `router.json` to `rayline-router` |
| `raydock opencode` | `arc-sage-1.0` | `small_model`: `arc-swift-1.0` | `-m rayline/rayline-router` |
| `raydock pi` | `arc-sage-1.0` | — | `--model rayline/rayline-router`, or pi's model picker |
| `raydock omp` | `arc-sage-1.0` (`default`, `slow`, `plan`) | `smol`: `arc-swift-1.0` | pick `rayline/rayline-router` for a role |
| `raydock hermes` | `arc-sage-1.0` | — | `--model rayline-router` (per run) |
| `raydock openclaw` | `arc-sage-1.0` | — | switch the session's model to `rayline/rayline-router` (per run) |

Each generated harness config lists all three models, so the harness's own
picker offers ARC Sage, ARC Swift and the Legacy Rules Router. A model you set
yourself is kept: `--model` (before or after `--`) or `ANTHROPIC_MODEL` for
Claude Code (subagents then inherit it, as before), a cloud-only `--config`'s
`routes.main` and `routes.subagent` models (with `--route all`; without a
`routes.subagent`, subagents follow `routes.main`), a `--router-config-path`
subagent allowlist (Raydock then sets no subagent model), your own `CLAUDE_CODE_SUBAGENT_MODEL` or
`ANTHROPIC_DEFAULT_HAIKU_MODEL`, a `--config` route, and the model pi and omp
saved. The pi `settings.json` and omp `config.yml` that Raydock seeds only once
are moved to ARC only while they still read exactly as an 0.2.x release wrote
them, which means no model was ever picked there. Raydock rewrites its model
keys in the Hermes and OpenClaw configs on every launch (as before), so pick
Legacy there per run.

The local and hybrid paths (`--local`, a local-plane `--config`, account-local
and may-local routing, `raydock hybrid run`) are unchanged: they keep the local
router's virtual model, `rayline-router`, which tells it to apply its own routes.
`raydock claude-desktop` already runs on ARC, and `raydock codex --auth
subscription` keeps your ChatGPT subscription for the main agent.

## OpenClaw

`raydock openclaw` runs OpenClaw against the Rayline router with a generated
config. The default `rayline` provider speaks OpenAI Chat Completions (OpenClaw's
own default protocol) with `compat.sendSessionAffinityHeaders`, so OpenClaw sends
its session id as `x-session-affinity` and the router keeps one routing episode
per OpenClaw session, including a `--session-id` resume in a later launch. A
second provider, `rayline-resp`, lists the same models over OpenAI Responses.
Hosted OpenAI tools such as `web_search` are not carried on Chat Completions;
ordinary function tools are.

The launcher gives the router key to OpenClaw as a file SecretRef that reads an
owner-only `router-key` file next to that config, so the key stays out of
OpenClaw's own files and out of the environment its tools inherit. The other
launchers (`raydock opencode`, `pi`, `omp`, `hermes`) likewise write the key into
their owner-only generated config, never into the harness's environment.

If you configure OpenClaw by hand, do the same. Put the key in a file only you
can read (`chmod 600`), declare it as a file secret provider, and write the
provider's `apiKey` as a SecretRef to it:

```json
"secrets": {
  "providers": {
    "rayline": { "source": "file", "path": "/absolute/path/to/router-key", "mode": "singleValue" }
  }
},
"models": {
  "providers": {
    "rayline": {
      "baseUrl": "https://api.rayline.ai/v1",
      "api": "openai-completions",
      "apiKey": { "source": "file", "provider": "rayline", "id": "value" },
      "models": [{ "id": "arc-sage-1.0", "compat": { "sendSessionAffinityHeaders": true } }]
    }
  }
}
```

Avoid a plain or `${VAR}` string: OpenClaw resolves it when it loads the config,
and then writes the resolved key into its agent model cache
(`~/.openclaw/agents/<agent>/agent/models.json`) in plain text. For a SecretRef
it writes only a marker there. Avoid an environment SecretRef too: every tool
OpenClaw runs inherits its environment, so a shell `env` would show the key.

## Choosing Where Requests Go

Routing comes down to three independent flags on `raydock claude`. You'll rarely
reach past `--local`; the other two are advanced overrides.

| Flag | What it controls | Values | Default |
| --- | --- | --- | --- |
| `--local` | **Who decides** — where routing decisions are made | flag (cloud when absent) | cloud (hosted) |
| `--via` | **How it connects** — how Claude Code reaches the router | `proxy`, `env` | `proxy` |
| `--route` | **What flows through** — what the proxy sends to the router | `all`, `subagents` | router-dependent (see below) |

### `--local` — who decides

- **Absent:** the **hosted cloud router** at `api.rayline.ai` makes routing and
  model decisions (requires `raydock auth login`).
- **Present:** the **on-device static router** decides locally. No login, and no
  routing decision leaves your machine. `--local` forces the proxy, since local
  inference is only reachable through it.

### `--via` — how it connects

How Rayline wires Claude Code to the router. Both are first-class Claude Code
injection points:

- **`proxy`** (default): a local proxy intercepts and forwards each request.
  **Required** to reach local inference or to route selectively. Also enables
  `raydock top` and per-request metrics.
- **`env`**: points `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` at the router for the
  whole session — lightest weight, no background process. **Cloud-only**: it
  can't reach local inference and can't route selectively, so it's rejected when
  combined with `--local` or `--route subagents`.

### `--route` — what flows through

What goes through the router versus straight to Anthropic:

- **`all`**: route every request (main agent + subagents).
- **`subagents`**: route only subagent traffic; the main agent stays on cloud
  Claude. This is the hybrid path — quality main agent, offloaded subagents.
  With the cloud router, Raydock sets `CLAUDE_CODE_SUBAGENT_MODEL=arc-swift-1.0`
  (unless you set it), so subagents run on ARC Swift. A routed subagent turn that
  still names a Claude model is sent as `rayline-router`, so the router picks its
  model; a subagent that names another model keeps it.

When the router serves the main agent (`--route all`), refusals are the
router's to handle, so Rayline turns off Claude Code's own refusal recovery:
it sets `CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK=1` (no resend to another Claude
model) and `CLAUDE_CODE_DISABLE_REFUSAL_RETRY=1` (no same-model resend). A value
you set yourself is kept. `--route subagents` leaves Claude Code's defaults.
In proxy mode (`--route all` and `--route subagents`) Rayline also sets
`CLAUDE_CODE_TETHER_LIVE=0`, turning off Claude Code's message threads: in
proxy mode Claude Code treats the router as first-party Anthropic and would
send later tool turns as continuations of server-held history, which the
router does not keep. With it off, every request carries the full
conversation. A value you set yourself is kept.
A running Claude Code daemon is reused only if it holds the same values (its
background agents inherit its env); otherwise Rayline treats it as a conflict.

The default depends on the router, because the two are used differently:

- **Cloud router → `all`.** The hosted router does model selection, so applying
  it everywhere is the intended behavior.
- **Local router → `subagents`.** Local sessions are hybrid by default; pass
  `--route all` to route the main agent as well. The default local config still
  sends main to cloud Claude; set `routes.main.endpoint` to `local` in a router
  config for a fully-local session.

### Common combinations

| Command | Who decides | How it connects | What flows through |
| --- | --- | --- | --- |
| `raydock claude` | cloud | proxy | all |
| `raydock claude --via env` | cloud | env | all |
| `raydock claude --local` | local | proxy | subagents |
| `raydock claude --local --route all` | local | proxy | all |
| `raydock claude --route subagents` | cloud | proxy | subagents |

`raydock claude --via env --local` and `raydock claude --via env --route subagents`
are rejected: the env mechanism is cloud-only and can't route selectively.

### Deprecated flags

These older flags still work for one release and print a one-line warning naming
the replacement:

| Deprecated | Use instead |
| --- | --- |
| `--local-router` | `--local` |
| `--no-proxy` | `--via env` |
| `--routing-mode override` | `--via env` |
| `--routing-mode proxy` | `--route all` |
| `--routing-mode proxy-subagents` | `--route subagents` |

## Signing In to Hosted Rayline

Skip this section if you only use `--local` — the local path never needs an
account.

Run `raydock auth login` to sign in. Browser login redeems a PKCE code through
`https://api.rayline.ai/v1/auth/cli/token`; device login uses the Rayline-native
`/v1/auth/cli/device/*` endpoints. The CLI stores Rayline-scoped opaque session
credentials — not Firebase or Google OAuth tokens.

Hosted Claude launches mint and store an `rlk-` router key separately from the
CLI session. Logging out calls `/v1/auth/cli/revoke` best-effort, clears the
local session, and drops the stored router key.

Custom hosted environments can be configured in
`~/.config/rayline/settings.json` for internal/dev routing.

## Routing Specific Subagents (Override Config)

The router config is JSON layered over Raydock's defaults. Save it wherever
you like — the examples below use `~/.config/rayline/local-router.json`.

Pass the file with `--router-config-path` when `routes.subagents` is meant to
limit which Claude Code subagents reach the local router. The local router can
also read `RAYLINE_ROUTER_CONFIG`, but the transparent proxy builds its subagent
allowlist **only** from `--router-config-path`.

This override keeps default Claude Code behavior as passthrough and routes only an
explicitly named `Explore` subagent to the active local model:

```json
{
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "local"
      }
    }
  }
}
```

Launch with that config:

```bash
raydock claude \
  --local \
  --isolated \
  --router-config-path ~/.config/rayline/local-router.json
```

Notes:

- `endpoint: "local"` means the managed local adapter. If `model` is omitted,
  Raydock fills in the active local model.
- With `--route subagents`, `routes.subagents` also becomes the proxy allowlist.
  The proxy matches the resolved Claude Code agent type case-insensitively, so
  `Explore` is enough once Claude Code has written subagent metadata. This
  allowlist behavior requires launching with `--router-config-path`; don't rely
  on `RAYLINE_ROUTER_CONFIG` alone for allowlists.
- If `routes.subagents` is omitted, all detected subagents can reach the local
  router. Main-thread Claude Code traffic still passes through normally under
  `--route subagents`.

## One Model For Everything (`routes.main` Only)

`routes.subagent` is optional. A config that declares `routes.main` and no
`routes.subagent` routes **subagents to the same target as main**, so a
single-model setup needs one route entry. This ships as
[`K.json`](../examples/routing-modes/K.json):

```json
{
  "endpoints": [
    {
      "id": "openrouter",
      "protocol": "anthropic_messages",
      "base_url": "https://openrouter.ai/api",
      "api_key_env": "OPENROUTER_API_KEY",
      "auth": "bearer",
      "models": ["moonshotai/kimi-k2.6"]
    }
  ],
  "routes": {
    "main": { "endpoint": "openrouter", "model": "moonshotai/kimi-k2.6" }
  }
}
```

```bash
export OPENROUTER_API_KEY=<your-api-key>
raydock codex app --config ./kimi.json     # or: raydock claude --config ./kimi.json
```

Resolution order for a subagent turn is: a matching `routes.subagents.<type>`
entry → `routes.subagent` → `routes.main` → `routes.default`. Add
`routes.subagent` only when subagents should differ from main (for example main
on a frontier model, subagents on-device):

```json
"routes": {
  "main": { "endpoint": "openrouter", "model": "moonshotai/kimi-k2.6" },
  "subagent": { "endpoint": "local" }
}
```

The shipped **`K-K`** mode
([`examples/routing-modes/K-K.json`](../examples/routing-modes/K-K.json)) is a
pure-keyed version of this split: one OpenRouter endpoint serving two models, with
`routes.main` on `kimi-k2.6` and `routes.subagent` on `glm-4.6`.

The same applies to the hosted cloud router: a config needs only its `main`
entry, and subagents then follow that model. The generated default
`~/.config/rayline/router.json` routes `main` to `arc-sage-1.0` and `subagent` to
`arc-swift-1.0`, and also lists `rayline-router` (the Legacy Rules Router) for a
route that names it. With a non-prod
`--env <name>` and no `--config`, `raydock codex` generates and reads
`~/.config/rayline/router.<name>.json` instead, pointed at that environment's
router, so a non-prod key never reaches the prod router. The file is created
once and never rewritten, with one exception: a file that is still exactly the
Legacy default an 0.2.x release generated (never edited) is upgraded to the ARC
default once, with a notice. Any edit, including a route you set to
`rayline-router`, keeps the file as it is. If you customized `router.json` for a
non-prod environment before, copy those routes into `router.<name>.json`.

## Connecting Provider Endpoints

Provider endpoints go in the same static router config. Keep API keys in env vars
so secrets never live in JSON. By default the local router sends `api_key_env` as
`x-api-key` for `anthropic_messages` endpoints and as bearer auth for
`openai_chat` and `openai_responses` endpoints. Set the optional per-endpoint
`auth` field (`"bearer"`, `"api_key"`, or `"client_bearer"`) to override that.
`client_bearer` forwards Codex's inbound `Authorization` and
`ChatGPT-Account-ID` headers to the selected upstream and is restricted to the
ChatGPT Codex backend or loopback development endpoints.

All protocols stream incrementally on their native path: `anthropic_messages`
forwards the upstream's native Anthropic SSE verbatim, `openai_chat` translates
the upstream OpenAI Chat SSE into Anthropic SSE for Claude Code, and
`openai_responses` forwards OpenAI Responses SSE for Codex/OpenAI-compatible
clients. `openai_chat` also forwards image blocks: Anthropic `image` blocks
become OpenAI `image_url` content parts (base64 sources become `data:` URLs).

When Codex talks to the local router, Rayline handles the provider paths Codex
uses today:

- `GET /v1/models`
- `POST /v1/responses`
- `POST /v1/responses/count_tokens`
- `POST /v1/responses/compact`
- `POST /v1/memories/trace_summarize`
- `POST /v1/images/generations`
- `POST /v1/images/edits`
- `POST /v1/alpha/search`

For routes that select an `openai_responses` endpoint, Rayline passes OpenAI
Responses-family requests through to the upstream and rewrites only the selected
model. For Anthropic, OpenAI Chat, or local-adapter routes, Rayline translates
Codex Responses create requests into the target protocol and synthesizes Codex
Responses SSE/JSON back to the client, including `compaction` output items for
Codex remote compaction turns.

**Codex subscription passthrough** uses Codex's own ChatGPT login. In a
`raydock codex --auth subscription --config <path>` config, the shared
`subscription` sentinel from Claude configs is materialized into this endpoint:

```json
{
  "endpoints": [
    {
      "id": "codex-subscription",
      "protocol": "openai_responses",
      "base_url": "https://chatgpt.com/backend-api/codex",
      "auth": "client_bearer",
      "models": ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"]
    }
  ],
  "routes": {
    "main": {
      "endpoint": "codex-subscription",
      "model": "gpt-6-astra"
    },
    "subagent": {
      "endpoint": "local",
      "model": ""
    }
  }
}
```

That shape keeps the main Codex turn on the user's subscription while allowing a
local or API-key route for subagents/model overrides.

**Anthropic-compatible endpoint:**

```json
{
  "endpoints": [
    {
      "id": "anthropic-compatible",
      "protocol": "anthropic_messages",
      "base_url": "https://anthropic-compatible.example.com",
      "api_key_env": "ANTHROPIC_COMPATIBLE_API_KEY",
      "models": ["claude-sonnet-5"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "anthropic-compatible",
        "model": "claude-sonnet-5"
      }
    }
  }
}
```

**OpenRouter** (recommended via its Anthropic-native endpoint). OpenRouter serves
`POST https://openrouter.ai/api/v1/messages`, a real Anthropic Messages endpoint
that returns native Anthropic SSE and supports images and tools. Use
`protocol: anthropic_messages` with `base_url: "https://openrouter.ai/api"` (the
router appends `/v1/messages`) and `auth: "bearer"` so the key is sent as
`Authorization: Bearer`. This gives true native streaming for free:

```json
{
  "endpoints": [
    {
      "id": "openrouter",
      "protocol": "anthropic_messages",
      "base_url": "https://openrouter.ai/api",
      "api_key_env": "OPENROUTER_API_KEY",
      "auth": "bearer",
      "models": ["anthropic/claude-sonnet-5"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "openrouter",
        "model": "anthropic/claude-sonnet-5"
      }
    }
  }
}
```

**OpenAI for Claude Code** (which has no Anthropic-native endpoint) uses the
`openai_chat` protocol with `https://api.openai.com/v1` and `OPENAI_API_KEY`.
The router opens the upstream with `stream: true` and translates the OpenAI Chat
SSE into Anthropic SSE in real time, so OpenAI also streams token by token:

```json
{
  "endpoints": [
    {
      "id": "openai",
      "protocol": "openai_chat",
      "base_url": "https://api.openai.com/v1",
      "api_key_env": "OPENAI_API_KEY",
      "models": ["gpt-6-astra"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "openai",
        "model": "gpt-6-astra"
      }
    }
  }
}
```

**OpenAI-compatible Responses for Codex** uses the `openai_responses` protocol
with a `/v1` base URL. The router opens `/responses`, uses bearer auth when
`api_key_env` is set, and streams Responses SSE through to Codex. Auxiliary
Codex provider endpoints under `/responses/*`, `/memories/trace_summarize`,
`/images/*`, and `/alpha/search` pass through on native `openai_responses`
routes.

**Arbitrary local OpenAI-compatible endpoint**, including a separately managed
llama.cpp server:

```json
{
  "endpoints": [
    {
      "id": "local-llama",
      "protocol": "openai_chat",
      "base_url": "http://127.0.0.1:8080/v1",
      "api_key_env": "LLAMA_API_KEY",
      "models": ["qwen3.6-35b-local"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "local-llama",
        "model": "qwen3.6-35b-local"
      }
    }
  }
}
```

If that local server needs no auth, omit `api_key_env`.

Direct endpoints in the router JSON only control where the local router sends
matching requests — they don't control the daemon's managed local adapter. An
explicit `raydock claude --local` launch still starts the managed local adapter
with the active local-model config; without a custom local-model config, that's
the bundled llama.cpp path.

To use Raydock's managed local adapter, route to `endpoint: "local"` as
shown above. If you want the managed adapter to use your own server and skip the
bundled GGUF model, configure a custom local model:

```bash
raydock local custom \
  --url http://127.0.0.1:8080 \
  --model qwen3.6-35b-local
```

The custom local-model endpoint must serve the Anthropic Messages API at
`/v1/messages`. The same config is stored under `local_model` in
`~/.config/rayline/settings.json`:

```json
{
  "local_model": {
    "mode": "custom",
    "base_url": "http://127.0.0.1:8080",
    "model": "qwen3.6-35b-local"
  }
}
```

## Where the Logs Live

Router state lives under `~/.rayline/rld`.

| Component | Path |
| --- | --- |
| Local-router supervisor, adapter, injector, and local-router logs | `~/.rayline/rld/rl-rld.log` |
| Managed llama.cpp stdout/stderr | `~/.rayline/rld/llama-server.log` |
| Shared transparent proxy log | `~/.rayline/rld/rl-rld-proxy.log` |
| Isolated transparent proxy log | `~/.rayline/rld/cc/rl-rld-proxy.log` |

`raydock router logs` prints the combined `rl-rld.log`. Use `tail` for the
component-specific files:

```bash
tail -f ~/.rayline/rld/rl-rld.log
tail -f ~/.rayline/rld/llama-server.log
tail -f ~/.rayline/rld/rl-rld-proxy.log
tail -f ~/.rayline/rld/cc/rl-rld-proxy.log
```

Isolated launches use `~/.rayline/cc` as `CLAUDE_CONFIG_DIR` and keep their proxy
state in `~/.rayline/rld/cc`. The model/router process stays shared in
`~/.rayline/rld`; an isolated launch starts a separate proxy sidecar when needed.

`llama-server.log` is present only when the active local-model config uses the
bundled managed llama.cpp path. A direct endpoint in the router JSON can route
matching requests elsewhere, but it doesn't by itself stop the managed adapter
from starting. Configure `raydock local custom` to make the daemon use
`--upstream-url` and skip the bundled GGUF server.

## Troubleshooting

If a local launch seems stuck, watch the combined router log and — for the
bundled managed llama.cpp path — the llama.cpp log:

```bash
tail -f ~/.rayline/rld/rl-rld.log
tail -f ~/.rayline/rld/llama-server.log
```

First model load can take a minute or two. If a custom local-model endpoint is
unreachable, check daemon status and probe it (Chat Completions, which hybrid
routing uses; Anthropic Messages, which `--local` Claude Code uses, is
reported too):

```bash
raydock router status
raydock local test --url http://127.0.0.1:8080 --model qwen3.6-35b-local
```

To force a clean restart:

```bash
raydock router stop
raydock claude --local --isolated --router-config-path ~/.config/rayline/local-router.json
```

If a turn fails with a provider-side error (Codex shows "Provider returned
error"), the router log now records the upstream status:

```
WARN rayline_local_router: upstream returned HTTP 400 (task=main selected=…)
```

The response body is passed through to the client untouched — read it there for
the provider's own message. Two payload shapes providers commonly reject are
already handled on the Codex path, so they are not the cause:

- Tool names namespaced with characters outside `[A-Za-z][A-Za-z0-9_-]*`
  (`server.tool`, `server/tool`) are rewritten on the way upstream and restored
  on the way back.
- Empty text blocks (a replayed assistant turn that was only a tool call, a
  cancelled stream) are pruned before the request is sent.

To validate the full end-to-end Claude Code path through Raydock, see
[Acceptance testing](acceptance-testing.md).
