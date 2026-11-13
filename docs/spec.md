# Rayline CLI specification

This is the contract for the `rayline` CLI: every command, option, config key, env var and file it has. Code implements exactly this. Anything not listed here does not exist: no extra flags, keys, env vars, aliases or files.

Sections marked **Internal** are real but stay out of public docs and `--help`. Sections marked **Kept for review** keep today's behaviour unchanged for now and get their own review later; they may not grow.

## 1. Rules

1. **Fail fast.** Rayline validates `settings.json`, `credentials.json`, env overrides and flags once, at load, and refuses to run with a clear error that names the key or flag. After load, the code trusts the parsed config: no checks, defaults or fallbacks at call sites.
2. **No silent fallback.** A missing or invalid value is an error. It is never skipped, guessed or replaced by another source.
3. **One spelling, one value.** Each key, flag and env var has one name and each meaning has one value. No aliases, no alternative spellings, no deprecated names.
4. **No compatibility code.** No migration of old files, keys, names or install layouts. Old files are ignored, not read and not moved.
5. **No hard-coded models in the local process.** The candidates ARC picks from, their context windows, output limits and prices come from ARC or from `settings.json`, never from constants in code. The model a harness asks for in cloud mode is today's default and is unchanged.
6. **No new surface.** Nothing is added that this spec does not list.

## 2. Command name

The installed binary is `raydock`. `rayline` is an alias for it (`~/.rayline/bin/rayline -> raydock`). Both work; this spec writes `rayline`. The alias code and its migration of old payloads stay until one name is picked (kept for review). The old `rl` alias is gone.

## 3. Public CLI

```
Usage: rayline [OPTIONS] COMMAND [ARGS]...

Options:
  --version        Show version
  --help           Show this message and exit

Commands:
  auth             Sign in, sign out, show status, print a token
  run              Launch a coding harness on ARC
  local            Start, stop or inspect the local Rayline process (supports hybrid mode)
  top              Show live request metrics
  update           Check for or install a Rayline update

Usage: rayline run <HARNESS> [--local | --config <file>] [HARNESS ARGS]...

Harnesses:
  claude           Claude Code
  claude-desktop   Claude Desktop's Code sessions
  codex            Codex CLI
  opencode         opencode
  pi               pi
  omp              oh-my-pi
  hermes           Hermes Agent
  openclaw         OpenClaw
```

### 3.1 `rayline auth`

| Command | Does |
|---|---|
| `rayline auth login` | Sign in with Rayline session login in the browser. Writes `credentials.json`. |
| `rayline auth logout` | Revoke the stored session and remove the stored router key. |
| `rayline auth status` | Show the signed-in account and environment. |
| `rayline auth token` | Print a valid account bearer token. |

`rayline auth login` options are **kept for review** (section 9).

### 3.2 `rayline run <HARNESS>`

```
rayline run <HARNESS> [--local | --config <file>] [HARNESS ARGS]...
```

1. With neither flag: **cloud mode**. Config from `~/.config/rayline/settings.json`. No local process. The harness talks to Rayline cloud directly.
2. `--local`: **local mode**. Config from `~/.config/rayline/settings.json`. The harness talks to the local process, which is started if it isn't running. The config must have a `routing` section (section 5.4).
3. `--config <file>`: config from `<file>` instead of the default file (it replaces it, never merges). If the file has a `routing` section the run is in local mode, otherwise cloud mode.
4. `--local` and `--config` together are a usage error.
5. Rayline options come first, right after the harness name. The first argument that is not a Rayline option, or everything after `--`, goes to the harness unchanged, including `--help`.
6. `rayline run --help` prints the usage above. `rayline run <HARNESS>` with an unknown harness is a usage error.
7. Each harness keeps today's behaviour in both modes, apart from what this spec removes.

Harness-specific options and subcommands are **kept for review** (section 9).

### 3.3 `rayline local`

| Command | Does |
|---|---|
| `rayline local start [--config <file>]` | Start the local process from the default config file, or from `<file>`. The config must have a `routing` section. |
| `rayline local status` | Show whether the local process is running, its address and the config file it was started from. |
| `rayline local stop` | Stop the local process. External inference servers keep running. |

1. One local process per machine, with its state in `~/.rayline/local/`.
2. `start` while it is running is an error that names the running process and its config file.
3. A `rayline run ... --local` or `--config` run whose config file differs from the one the running process was started from is an error. It never silently uses the running process's config.

### 3.4 `rayline top`

```
rayline top [--json]
```

1. Shows live request metrics from the local process.
2. `--json` prints one snapshot as JSON and exits.
3. With no local process running it says so and exits with an error.

### 3.5 `rayline update`

```
rayline update
rayline update --check
```

Its options are **kept for review** (section 9). The channel comes from `update.channel` (section 5.6).

## 4. Internal CLI

1. `--env <name>`, hidden, a global option placed before the command: `rayline --env dev run claude`. It selects `environments.<name>` from `settings.json` and from `credentials.json`. Without it, prod (built in). An unknown name is a load error.
2. `rayline update` hidden release-testing options: `--base-url <url>` (where releases are read from) and `--install-path <path>` (which binary is replaced). They replace the env vars `RAYLINE_UPDATE_BASE_URL` and `RAYLINE_UPDATE_INSTALL_PATH`.
3. The local process runs from the `raydock` binary itself, through an internal entry point that `rayline local start` uses. It is not a user command and is not in `--help`. There is no separate `rld` binary.

## 5. `settings.json`

One file: `~/.config/rayline/settings.json`, written by the user. `--config <file>` takes a file of the same shape. The file is optional: with no file, every section is empty.

```json
{
  "endpoints": {
    "gpu-box": {
      "base_url": "https://gpu-box.example.com:8443",
      "protocols": ["anthropic_messages", "openai_chat", "openai_responses"],
      "api_key_env": "GPU_BOX_API_KEY"
    },
    "ollama": {
      "base_url": "http://127.0.0.1:11434",
      "protocols": ["openai_chat", "anthropic_messages"]
    }
  },
  "models": {
    "z-ai/glm-5.3-flash": {
      "endpoint": "gpu-box",
      "model": "GLM-5.3-Flash-EXL3",
      "context_window": 1000000,
      "max_output_tokens": 32000,
      "tokenizer": "zai-org/GLM-5.3-Flash",
      "price": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0 }
    }
  },
  "routing": {
    "decisions_model": "arc-swift-1.0",
    "classifier_model": "anthropic/claude-haiku-5.5",
    "policy": {
      "preset": "arc-policy.prefer-local.v1",
      "overrides": {
        "selection": { "min_p_pass": 0.5 },
        "schedule": { "model": ["cache_boundaries", "user_messages"] }
      }
    }
  }
}
```

### 5.1 Rayline cloud

Rayline cloud (prod) is built in and has no entry. It serves every protocol a harness speaks. Login is always Rayline session login.

### 5.2 `endpoints`

Your own inference servers, keyed by id.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `base_url` | string (URL) | yes | The server's address. |
| `protocols` | list of `anthropic_messages`, `openai_chat`, `openai_responses` | yes, non-empty | The wire protocols it serves. |
| `api_key_env` | string | no | Name of the env var holding its key. No secrets in the file. |

Each harness speaks one protocol: `claude` and `claude-desktop` speak `anthropic_messages`, `codex` speaks `openai_responses`, the others speak `openai_chat`. When the endpoint lists the harness's protocol, requests pass straight through with no translation. When it doesn't, the config is wrong and Rayline refuses to run.

### 5.3 `models`

Models ARC can pick that are not served by Rayline cloud, keyed by the model id ARC returns.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `endpoint` | string | yes | An id in `endpoints`. |
| `model` | string | yes | The name that server knows the model by. |
| `context_window` | integer > 0 | yes if ARC doesn't know the model | Context window in tokens. |
| `max_output_tokens` | integer > 0 | yes if ARC doesn't know the model | Output ceiling in tokens. |
| `tokenizer` | string | no | Hugging Face tokenizer repo for token counting. |
| `price` | object `input`, `output`, `cache_read`, `cache_write` (numbers >= 0) | no | Overrides ARC's price for this model only. |

1. When ARC picks a model in `models`, the local process sends the request to its endpoint under its `model` name.
2. Any model ARC picks that is not in `models` goes to Rayline cloud under the id ARC returned. Its limits and price come from ARC.
3. Nothing marks a model as "local". Every entry works the same way.
4. Main/subagent routes are not supported.

### 5.4 `routing`

Required for local mode. Its presence in a `--config` file selects local mode.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `decisions_model` | string | yes | Which ARC product makes decisions (for example `arc-swift-1.0`). No default in code. |
| `classifier_model` | string | yes | The model that serves Claude Code's auto-mode safety classifier calls, sent to Rayline cloud. No default in code. |
| `policy.preset` | non-empty string | yes | ARC's preset id (`arc-policy.package-default.v1`, `arc-policy.hybrid-default.v1` or `arc-policy.prefer-local.v1`). Sent to ARC unchanged; no short names. |
| `policy.overrides` | object | no | ARC's own `routing.policy.overrides` shape, sent to ARC unchanged. ARC validates it. |

### 5.5 Internal: `environments`

Not in public docs. Prod is built in. Internal users add other environments and pick one with `--env`.

```json
"environments": {
  "dev": {
    "base_url": "https://<dev API host>",
    "login_url": "https://<dev platform host>/cli-auth"
  }
}
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `base_url` | string (URL) | yes | The Rayline API for that environment. |
| `login_url` | string (URL) | yes | The page `rayline auth login` opens. |

### 5.6 Internal: `update`

Not in public docs.

```json
"update": { "channel": "dev" }
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `channel` | `prod`, `dev` or `main` | no, default `prod` | The release channel `rayline update` follows: `prod` is public releases, `dev` release candidates, `main` a build from every merge to main. |

The channel the binary was built on is not used as a default.

### 5.7 Env overrides

Each scalar key Rayline defines can be overridden by one env var named `RAYLINE_<SECTION>_<KEY>`, built from its path with single underscores:

| Key | Env var |
|---|---|
| `routing.decisions_model` | `RAYLINE_ROUTING_DECISIONS_MODEL` |
| `routing.classifier_model` | `RAYLINE_ROUTING_CLASSIFIER_MODEL` |
| `routing.policy.preset` | `RAYLINE_ROUTING_POLICY_PRESET` |
| `update.channel` | `RAYLINE_UPDATE_CHANNEL` |

1. Precedence, lowest to highest: built-in defaults, the config file, env overrides, flags.
2. An override value is validated like the file value; an invalid one is a load error.
3. Whole entries (`endpoints.*`, `models.*`, `environments.*`) and ARC's `policy.overrides` have no env override. Put them in a file and pass it with `--config`.
4. No other `RAYLINE_*` env var changes Rayline's behaviour (section 7).

### 5.8 Load errors

Load refuses, naming the key:

1. An unknown key at any level Rayline defines.
2. A wrong type, an empty string, or a value out of range.
3. In a local-mode run, an endpoint that doesn't list the protocol of the harness being run. Cloud mode doesn't use endpoints, and `rayline local start` has no harness, so neither checks this.
4. A model entry whose `endpoint` is not in `endpoints`.
5. Local mode without `routing`, or `routing` missing a required key.
6. `--env <name>` with no `environments.<name>` entry. `prod` gets no special case.
7. A routing env override when the file has no `routing` section. The mode comes from the file only.
8. A `--config <file>` that doesn't exist. Only the default file is optional.
9. A key repeated in one JSON object, an empty map key, a URL that isn't absolute `http` or `https`, a duplicate in `protocols`, or a `price` missing any of its four fields.

When the local process starts, before it serves anything, it also refuses:

10. An `api_key_env` naming an env var that isn't set.
11. A model entry ARC doesn't know that is missing `context_window` or `max_output_tokens`.

## 6. `credentials.json`

One file: `~/.config/rayline/credentials.json`. `rayline auth login` writes `session` and `router_key` together (it mints the router key at sign-in), `rayline auth logout` removes them, and a token refresh rewrites `session` only, because refresh tokens rotate. Nothing else writes it. It is the only source for Rayline's session and router key: no env var or flag overrides it.

```json
{
  "session": {
    "email": "you@example.com",
    "subject": "uid_abc123",
    "access_token": "eyJ...",
    "access_token_expires_at": "2026-11-12T11:00:00Z",
    "refresh_token": "eyJ...",
    "refresh_token_expires_at": "2026-12-12T10:00:00Z",
    "signed_in_at": "2026-11-12T10:00:00Z"
  },
  "router_key": { "api_key": "rlk-...", "subject": "uid_abc123", "saved_at": "2026-11-12T10:00:05Z" },
  "environments": {
    "dev": {
      "session": { "...same shape..." },
      "router_key": { "...same shape..." }
    }
  }
}
```

1. Prod sits at the top level. `environments.<name>` holds the same two objects for an internal environment.
2. snake_case only. No `version` field, no `authKind`.
3. Each slot (the top level for prod, or `environments.<name>`) is either absent, meaning signed out, or holds both objects with every field shown. A slot with only one object, or a missing or invalid field, is a load error that tells the user to run `rayline auth login`. A missing file means signed out everywhere.
4. A file in today's shape is invalid: it is a load error that tells the user to run `rayline auth login`. It is not migrated.

## 7. Env vars

### 7.1 Read by Rayline

1. The settings overrides in section 5.7.
2. The env var each endpoint's `api_key_env` names.
3. OS: `PATH`, `NO_COLOR`.
4. Harness homes, read to find a harness's own files: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CONFIG_DIR`, `XDG_STATE_HOME`, `OPENCLAW_CONTAINER`.
5. Values the user set for a harness, passed on unchanged: `NODE_USE_SYSTEM_CA`, `CLAUDE_CODE_AUTO_MODE_SERVER`, `CLAUDE_CODE_DISABLE_AGENT_VIEW`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
6. Shared Hugging Face cache: `HF_HOME`, `HF_HUB_CACHE`.

### 7.2 Set by Rayline for a harness

1. Claude Code: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`, `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK`, `CLAUDE_CODE_DISABLE_REFUSAL_RETRY`, `CLAUDE_CODE_TETHER_LIVE`, `RAYLINE_ENV_NAME`.
2. Other harnesses: `HERMES_HOME`, `PI_CODING_AGENT_DIR`, `PI_PROFILE`, `OMP_PROFILE`, `OPENCODE_CONFIG`, `OPENCLAW_CONFIG_PATH`.
3. Removed from the harness environment: `ANTHROPIC_API_KEY`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`.

### 7.3 Internal: handoff between Rayline's own processes

Set by one Rayline process and read only by a child it starts. Never read from the user's shell:

1. `RAYLINE_EXEC_AUTH_TOKEN`, `RAYLINE_EXEC_PARENT_PIPE`, `RAYLINE_TOKEN_COUNTER_KEY`, `RAYLINE_HYBRID_GATEWAY_TOKEN`, `RAYLINE_ROUTER_API_KEY`.

### 7.4 Build time only

`RAYLINE_VERSION`, `RAYLINE_CHANNEL`, `RAYLINE_DAEMON_VERSION`, `RAYLINE_DAEMON_CHANNEL`. Not read at runtime.

### 7.5 Test builds only

`RAYDOCK_TEST_DETACH_HELPER`, `PI_TEST_SESSION_OWNERS_DIR`, `RAYLINE_CLAUDE_DESKTOP_NO_LAUNCHD`. Compiled only into test builds, not into the shipped binary.

Every other env var Rayline reads today is gone (section 10).

## 8. Files

### 8.1 `~/.config/rayline`

Only two files:

1. `settings.json`: written by the user.
2. `credentials.json`: written by `rayline auth login` and `rayline auth logout`.

### 8.2 `~/.rayline`

```
~/.rayline/
  bin/                          raydock, rayline -> raydock, rayline-core
  local/                        local process state
    core-config.json            config handed to the core
    gateway-token               secret shared by the CLI and the local process
    ready.json                  local process address once it is up
    supervisor.json             what is running
    runtime.lock                one local process at a time
    runtime.log                 its log
  harness/<name>/               files Rayline generates per harness: pi, hermes, omp,
                                opencode, openclaw, codex, codex-app,
                                claude-desktop, claude-code (only with --isolated)
  harness/claude-code/daemon-launches.json
                                Claude Code daemons Rayline started
  harness/claude-desktop/claude-desktop.lock
                                one claude-desktop command changing Desktop at a time
```

Rayline writes nothing else, anywhere. Tests use a temporary home folder.

## 9. Kept for review

These keep today's behaviour for now, under the new command shape. Their aliases and extra values are still removed (section 10.4).

1. `rayline auth login`: `-b, --no-browser` (device-code login) and `-p, --paste` (paste the callback URL). `--paste` with `--no-browser` behaves as today (`--paste` wins). Launching a harness while signed out signs in on the spot with the default flow.
2. `rayline update`: `--check`, `--version <version>`, `--force`, `--dry-run`.
3. `rayline run claude --isolated`: gives Claude Code its own config folder (`~/.rayline/harness/claude-code/`) so it can run beside other sessions.
4. `rayline run codex`: `--model <model>` and `--auth auto|subscription|none`; the subcommands `rayline run codex app [WORKSPACE]` (launches the Codex desktop app on its own `CODEX_HOME`, `~/.rayline/harness/codex-app/`) and `rayline run codex configure` (writes a Codex profile to `$CODEX_HOME/rayline.config.toml`).
5. `rayline run claude-desktop [COMMAND]`: `start` (default), `status`, `validate`, `stop`, `install-service`, `uninstall-service`, `off`, `uninstall`, and `--manual`, `--prefixed-ids`, `--desktop-config <path>`. Desktop runs on the shared local process; its own router copy is gone. Its model ids are unchanged.

## 10. Removed

Everything below is deleted outright: code, tests, docs and help text.

### 10.1 Commands and options

1. `raydock router` and all its subcommands (`start`, `status`, `logs`, `top`, `stop`).
2. `raydock hybrid` and all its subcommands and options (`start`, `status`, `stop`, `run`, `--core`, `--config`, `--env`, `--local-context-window`, `--classifier-model`, `--cell`, `--port`, `--startup-timeout-secs`, `--state-dir`). `rayline local` and `rayline run --local` replace it.
3. Every subcommand of today's `raydock local`: `models`, `download`, `use`, `remove`, `custom`, `show`, `test`, `clear`, `on`, `off`, `onboard`, `subagents`, with `--url` and `--base-url`.
4. Top-level harness commands (`raydock claude`, `raydock codex`, `raydock claude-desktop`, `raydock pi`, ...) and the `raydock claude run` form. `rayline run <HARNESS>` replaces them.
5. `raydock claude` options: `-m, --model`, `--auto-compact-window`, `--local-provider`, `--route`, `--via`, `--local-injector-port`, `--statusline`, `--no-statusline`, `--diagnose`, `--upstream-ca-path`, `--router-config-path`, `--routing-mode`, `--no-proxy`, `--local-router`, `--no-telemetry`.
6. `--auth-token`, at the top level and on every command.
7. Top-level `raydock status`. `rayline auth status` is the only status command.
8. `top` options `--legacy`, `--hybrid`, `--all`, `--state-dir`, and the `a` key on the live screen.
9. `update --channel`. `update.channel` and `RAYLINE_UPDATE_CHANNEL` replace it.
10. The `rl` alias.

### 10.2 The old local stack

1. The old local router, transparent proxy, injector, adapter and the `rld` binary with its `serve`, `proxy`, `statusline` and `models` commands. The local process gateway moves into `raydock` (section 4.3).
2. Proxy mode and its proxy CA (`HTTPS_PROXY` injection, `NODE_EXTRA_CA_CERTS`).
3. The on-device model manager: downloads, hardware fit, llama.cpp management, Ollama/LM Studio discovery, account `enable_local` switching, subagent discovery and routes.
4. claude-desktop's own router copy (its state folder, ports, pid file, LaunchAgent and `router.json`).
5. Workspace crates used only by the above.

### 10.3 Settings and files

1. Settings keys: `router_url`, `cli_auth_url`, `account_url`, `default_env`, `auth_kind`, `authKind`, `firebase_api_key`, `google_device_client_id`, `google_device_client_secret`, `local_model`, `hybrid` (all of it, including `local_registry_model`, `routing`, `local_tokenizer`, `routing.local_price`, `cloud_output_tokens`, `classifier_model`, `local_context_window`), `subagent_routes`, `onboarding`.
2. Files: `~/.config/rayline/cli.toml`, `router.json`, `router.<env>.json`, `claude-daemon-launches.json` and harness homes under `~/.config/rayline`; `~/.rayline/rld/`, `~/.rayline/hybrid/`, `~/.rayline/cc/`, `~/.rayline/codex-app-home/`, `~/.rayline/claude-desktop/` and `~/.rayline/claude-desktop.lock`. Rayline neither reads nor moves them.
3. The old settings file location and its migration.
4. Today's `credentials.json` shape (`version`, camelCase fields, `authKind`, `router_keys.<env>`, `apiKey`).

### 10.4 Aliases, extra values and guesses

1. Firebase login and everything behind `auth_kind`.
2. A missing `refresh_token` replaced by the access token.
3. Sign-in flow guessed from `SSH_CONNECTION`, `DISPLAY`, `WAYLAND_DISPLAY` or `BROWSER`. `--no-browser` is the explicit way.
4. API mode aliases (`claude`, `openai`, `responses`, `openai_responses`): one name per mode.
5. Codex auth mode aliases (`no-auth`, `local`, `codex-subscription`, `chatgpt`, `codex`) and `--subscription`: only `auto`, `subscription`, `none`.
6. `n_ctx` as a context-window alias.
7. "Legacy mode" for the `current`/`execution_context_v1` API modes, support for a core without the token-counter contract, and legacy adapter overrides.
8. The Decisions environment guessed from its URL.
9. A missing context window replaced by a default.
10. A failure to get the router key hidden as "no key".
11. A missing classifier key or model silently skipping the classifier route.
12. The model taken from `--model`, then the request, then `ANTHROPIC_MODEL`.
13. Lowercase proxy env var fallbacks.
14. Hard-coded model constants: `CLOUD_MODELS`, `CLOUD_CONTEXT_WINDOW`, `DECISIONS_MODEL`, `LOCAL_REGISTRY_MODEL`, `DEFAULT_LOCAL_CONTEXT_WINDOW`, `CLASSIFIER_MODEL`. ARC chooses from its full model set.
15. The second copy of ARC's preset list in the core.
16. Install leftovers: cleanup of the old uv install and handling for releases from before the core.

### 10.5 Env vars

1. Overrides of `credentials.json`: `RAYLINE_AUTH_TOKEN`, `RAYLINE_ID_TOKEN`, `RAYLINE_ROUTER_API_KEY` from the user's shell, `RAYLINE_KEY` and its scrubbing.
2. `CLAUDE_SETTINGS_PATH`, `RAYLINE_HARNESS_CONV_ID_FALLBACK`, `RAYLINE_CONV_ID`, `RAYLINE_PI_ALLOW_EMPTY_SIGNATURE`, `ANTHROPIC_MODEL` as a model source, `RAYLINE_CALLER_CWD`.
3. `RAYLINE_CLI_AUTH_URL`, `RAYLINE_MODELS_REGISTRY_URL`. `environments` covers them.
4. `RAYLINE_UPDATE_BASE_URL`, `RAYLINE_UPDATE_INSTALL_PATH`. Hidden `update` options replace them (section 4.2).
5. The old local router, `rld` and proxy mode: `RAYLINE_MODEL_REPO`, `RAYLINE_MODEL_FILE`, `RAYLINE_MODEL_REVISION`, `RAYLINE_MODEL_SHA256`, `RAYLINE_LLAMA_TAG`, `RAYLINE_CTX_SIZE`, `RAYLINE_LOCAL_MODEL_ID`, `RAYLINE_NO_LOCAL_MODEL`, `RAYLINE_ADAPTER_PORT`, `RAYLINE_ADAPTER_UPSTREAM_MODEL`, `RAYLINE_ADAPTER_UPSTREAM_URL`, `RAYLINE_INJECTOR_PORT`, `RAYLINE_PROXY_PORT`, `RAYLINE_ISOLATED_PROXY_PORT`, `RAYLINE_ROUTER_URL`, `RAYLINE_DECISION_PLANE`, `RAYLINE_LOCAL_ROUTER_PORT`, `RAYLINE_ROUTER_CONFIG`, `RAYLINE_DATA_DIR`, `RAYLINE_PROXY_CA_CERT_PATH`, `RAYLINE_PROXY_CA_KEY_PATH`, `RAYLINE_UPSTREAM_CA_FILE`, `RAYLINE_ROUTE_STATUS_PATH`, `RAYLINE_ANTHROPIC_URL`, `RAYLINE_PROXY_ROUTING_MODE`, `RAYLINE_CLAUDE_ROUTING_MODE`, `RAYLINE_METRICS_PORT`, `RAYLINE_METRICS_URL`, `RLD_BIN`; proxy injection of `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`, `NODE_EXTRA_CA_CERTS`; the old router config folder from `APPDATA` and `XDG_CONFIG_HOME`; `OLLAMA_HOST` and `LMSTUDIO_HOST`.

## 11. Done means tested

1. Unit tests cover every load error in section 5.8 and section 6, every usage error, and the env override table.
2. Every harness in section 3 runs in cloud mode and in local mode and completes a real turn, as it does today.
3. The default test suite needs no live credentials or external services. Live runs are documented commands, run before merge.
