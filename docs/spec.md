# Rayline CLI specification

This is the contract for the `rayline` CLI: every command, option, config key, env var and file it has. Code implements exactly this. Anything not listed here does not exist: no extra flags, keys, env vars, aliases or files.

Sections marked **Internal** are real but stay out of public docs and `--help`. Sections marked **Kept for review** keep today's behaviour unchanged for now and get their own review later; they may not grow.

## 1. Rules

1. **Fail fast.** Rayline validates `settings.json`, env overrides and flags once, at load, before any command runs. Every command that uses `credentials.json` (`rayline auth status`, `rayline auth logout`, a harness launch, starting the local process) loads and validates it before it acts; `rayline local status`, `rayline local stop`, `rayline top` and `rayline update` don't read it. A local-mode launch that reuses a running local process reads only that process's state (`supervisor.json`, `gateway-token`), not `credentials.json`. `rayline auth login` replaces an invalid `credentials.json` with the new session and says so, because signing in is how the user repairs it (section 6). Otherwise an invalid value stops Rayline with a clear error that names the key or flag. After load, the code trusts the parsed config: no checks, defaults or fallbacks at call sites.
2. **No silent fallback.** A missing or invalid value is an error. It is never skipped, guessed or replaced by another source.
3. **One spelling, one value.** Each key, flag and env var has one name and each meaning has one value. No aliases, no alternative spellings, no deprecated names.
4. **No compatibility code.** No migration of old files, keys, names or install layouts. Old files are ignored, not read and not moved.
5. **No hard-coded models in the local process.** The candidates ARC picks from, their context windows, output limits and prices come from ARC or from `settings.json`, never from constants in code, except the three cloud models in section 5.3 item 2, which stay until the Rayline router publishes ARC's model set. The model a harness asks for in cloud mode is today's default and is unchanged.
6. **No new surface.** Nothing is added that this spec does not list.

## 2. Command name

The installed binary is `rayline` (`~/.rayline/bin/rayline`). It has no other name: no alias, symlink or link to it is installed.

## 3. Public CLI

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

Usage: rayline run <HARNESS> [--local | --config <file>] [HARNESS ARGS]...

Harnesses:
  claude           Claude Code
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
8. Local mode refuses these kept options and commands (section 9) before anything starts: `rayline run claude --isolated`, `rayline run codex app`, `rayline run codex configure`, and `rayline run codex` with `--model`. `app` and `configure` are Rayline commands only as the word right after `rayline run codex`. After Rayline's options they are codex arguments, so `rayline run codex --local app` passes `app` to codex.
9. For `claude`, `codex`, `pi` and `opencode`, local mode refuses a command line that resumes or forks a saved session, before anything starts. It looks at the harness arguments before any `--`, in both the `--flag` and `--flag=value` forms, and refuses these:
   - `claude`: `-r`, `-c`, `--resume`, `--continue`, `--fork-session`, `--resume-session-at`;
   - `codex`: `resume` or `fork` as the first argument, or `exec resume` as the first two;
   - `pi`: `-r`, `-c`, `--resume`, `--continue`, `--fork`, `--session`, `--session-id`;
   - `opencode`: `-c`, `-s`, `--continue`, `--session`, `--fork`.

   The arguments of other harnesses are not checked: `hermes --resume` and `openclaw --session-id`, for example, are passed on.
10. Local mode refuses OpenClaw container mode, before anything starts: `rayline run openclaw` when `OPENCLAW_CONTAINER` is set to a non-empty value, or when any harness argument is `--container` or `--container=<value>`.

Harness-specific options and subcommands are **kept for review** (section 9).

### 3.3 `rayline local`

| Command | Does |
|---|---|
| `rayline local start [--config <file>]` | Start the local process from the default config file, or from `<file>`. The config must have a `routing` section. |
| `rayline local status` | Show whether the local process is running, its address, the config file and the environment it was started from. |
| `rayline local stop` | Stop the local process. External inference servers keep running. |

1. One local process per machine, with its state in `~/.rayline/local/`.
2. `start` while it is running is an error that names the running process and its config file.
3. A `rayline run ... --local` or `--config` run whose config file differs from the one the running process was started from, or whose `--env` differs, is an error. It never silently uses the running process's config.
4. On macOS, `rayline local` and local-mode runs require macOS 13 or later.
5. Starting the local process downloads the tokenizer of each `models` entry that names one (section 5.3) into the Hugging Face cache (section 8.2). When Hugging Face doesn't answer within 20 seconds or a file fails to download, it uses the last complete copy in the cache and says that it is the cached copy. With no complete cached copy, start fails with an error that names the config file, the `models` entry and the tokenizer.
6. The token counter reads the tokenizer repo's files in the layouts Hugging Face models publish:
   - `tokenizer.json`;
   - the chat template: `chat_template.jinja`, else the `chat_template` key of `tokenizer_config.json` (a string, or named templates `default` and `tool_use`);
   - image sizing: `processor_config.json`, else `preprocessor_config.json`. In its `image_processor` object, else at its top level: `patch_size` and `merge_size`, and the image limits `min_image_tokens` and `max_image_tokens`, else `min_pixels` and `max_pixels`, else `size.min_pixels` and `size.max_pixels`, else `size.shortest_edge` and `size.longest_edge`.

   An image given only by URL, or whose bytes don't decode as an image, counts as the processor's maximum.
7. The local process passes each request on with a session id and a branch id:
   - session: the request's `x-rayline-session-id`, else the first of `x-conversation-id`, `x-parent-session-id`, `x-session-affinity`, `x-session-id`, `x-claude-code-session-id` that it carries, else none;
   - branch: the request's `x-rayline-branch-id`, else `thread-id`, else `x-claude-code-agent-id`, else, only when the request carries `x-parent-session-id`, its `x-session-affinity`, else its `x-session-id`; else `main`.
8. When the core fails a turn before any output because its call to the routing service failed, the local process resends the turn once, and only when the service was unavailable (no answer, or HTTP 408, 409 or 5xx) or busy (HTTP 429). It doesn't resend when the service says `x-should-retry: false` or a `retry-after` over 30 seconds. Before the resend it waits the service's `retry-after` (at least 1 second), else about 5 seconds, jittered between 2.5 and 7.5 so sessions don't resend in step.

   When the turn still fails, or isn't resent, the harness gets the error with `x-should-retry: false`, so its SDK doesn't resend it again:
   - unavailable: 503, and busy: 429. Both carry `retry-after`: the service's (which the core caps at 300 seconds, and the local process raises to at least 1), else 30 seconds.
   - a rejected router key: 403, and any other refusal: 400. Neither is resent, and neither carries `retry-after`.

   The core passes on the service's two headers separately: `Retry-After` only as delta-seconds, and `X-Should-Retry` only as exactly `true` or `false`. A refusal can carry one without the other, and the local process uses the one it gets. A missing header takes its own default, and only for an unavailable or busy service. Without `retry-after`, the local process waits about 5 seconds for its resend and tells the harness `retry-after: 30`. Without `x-should-retry`, the turn may be resent. When the service doesn't answer, the core passes on neither.

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

`rayline update` refuses Homebrew and Rayline.app installs: it replaces nothing and exits 0, with or without `--check`. When the binary it would replace is inside a `.app` bundle, it prints ``rayline is managed by Rayline.app. Update the app to update the CLI:`` and, on the next line, ``re-download from https://get.rayline.ai``. When that binary is under `brew --prefix` (with `brew` on `PATH`), it prints ``Installed via Homebrew. Run `brew upgrade rayline` to update.``

### 3.6 Installers

`scripts/install-rayline.sh` (macOS and Linux) and `scripts/install-rayline.ps1` (Windows) install `rayline`, `rayline-core` and `rayline-core-LICENSE`. Their public options:

1. `install-rayline.sh`:
   - `--version <version>`: the release to install, a bare version such as `0.5.0`. A version with a leading `v` is rejected. Without it, the installer takes the latest GitHub release.
   - `--install-dir <path>`: the directory the files go into. Default `~/.rayline/bin`. It replaces `RAYLINE_INSTALL_DIR`, which is not read.
2. `install-rayline.ps1`: `-Version <version>` and `-InstallDir <path>`, with the same rules.
3. To pass options to a piped installer:

```
curl -fsSL https://raw.githubusercontent.com/rayline-ai/raydock/main/scripts/install-rayline.sh | sh -s -- --version 0.5.0 --install-dir ~/bin
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/rayline-ai/raydock/main/scripts/install-rayline.ps1))) -Version 0.5.0 -InstallDir C:\tools\rayline
```

The hidden `--base-url` option is in section 4.4.

## 4. Internal CLI

1. `--env <name>`, hidden, a global option placed before the command: `rayline --env dev run claude`. It selects `environments.<name>` from `settings.json` and from `credentials.json`. Without it, prod (built in). An unknown name is a load error. `rayline local status`, `rayline local stop` and `rayline top` act on the one running local process, so `--env` with them is a usage error.
2. `rayline update` hidden release-testing options: `--base-url <url>` (where releases are read from) and `--install-path <path>` (which binary is replaced). They replace the env vars `RAYLINE_UPDATE_BASE_URL` and `RAYLINE_UPDATE_INSTALL_PATH`.
3. The local process runs from the `rayline` binary itself, through an internal entry point that `rayline local start` uses. It is not a user command and is not in `--help`. There is no separate `rld` binary.
4. `install-rayline.sh` hidden release-testing option `--base-url <url>`: where the release is read from. With `--version` it is a usage error. It replaces `RAYLINE_RELEASE_BASE_URL`.

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

In local mode each harness speaks one protocol, the only one its Rayline config sets up: `claude`, `pi`, `omp` and `opencode` speak `anthropic_messages`; `codex` and `hermes` speak `openai_responses`; `openclaw` speaks `openai_chat`. When the endpoint lists the harness's protocol, requests pass straight through with no translation. When it doesn't, the config is wrong and Rayline refuses to run. In cloud mode the config Rayline writes for `hermes` defaults to `anthropic_messages`, with an `openai_responses` provider beside it, and the one for `openclaw` adds an `openai_responses` provider beside `openai_chat`. Every other harness uses the same protocol in both modes.

### 5.3 `models`

Models ARC can pick that are not served by Rayline cloud, keyed by the model id ARC returns.
The action ARC identifies is the model id after its provider prefix (`qwen/qwen3.8-27b` -> `qwen3.8-27b`).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `endpoint` | string | yes | An id in `endpoints`. |
| `model` | string | yes | The name that server knows the model by. |
| `context_window` | integer > 0 | yes, when the local process starts (item 5) | Context window in tokens. |
| `max_output_tokens` | integer > 0 | yes, when the local process starts (item 5) | Output ceiling in tokens. |
| `tokenizer` | string | no | Hugging Face tokenizer repo for token counting. |
| `price` | object `input`, `output`, `cache_read`, `cache_write` (numbers >= 0) | no | Overrides ARC's price for this model only. |

1. When ARC picks a model in `models`, the local process sends the request to its endpoint under its `model` name.
2. The only models ARC can pick that are not in `models` are these three Rayline cloud models. When ARC picks one, it goes to Rayline cloud. A request over Messages carries its Rayline cloud id, and one over Chat Completions or Responses carries its id in the core's registry:

   | Rayline cloud id | Id in the core's registry | Context window | Output ceiling | Input modalities |
   |---|---|---|---|---|
   | `claude-opus-5-5` | `anthropic/claude-opus-5.5` | 1,000,000 | 128,000 | `["text"]` |
   | `claude-sonnet-5-5` | `anthropic/claude-sonnet-5.5` | 1,000,000 | 128,000 | `["text"]` |
   | `claude-haiku-5-5` | `anthropic/claude-haiku-5.5` | 1,000,000 | 128,000 | `["text"]` |

   A `models` entry keyed by one of the registry ids replaces that cloud model. ARC prices the cloud models. This list goes when the Rayline router publishes ARC's model set (section 10.4 item 14).
3. Nothing marks a model as "local". Every entry works the same way.
4. Main/subagent routes are not supported.
5. ARC doesn't publish the models it knows yet, so it knows no model's limits. Every entry must state `context_window` and `max_output_tokens`, and the local process refuses to start without them (section 5.8 item 11).
6. In local mode Rayline tells every harness a context window of 1,000,000 tokens, whichever model serves the turn.

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
| `base_url` | string (URL) | yes | The Rayline API for that environment: `https`, or `http` on a loopback host, with no `user:password@`. Any other URL is a load error naming the key and the URL. |
| `login_url` | string (URL) | yes | The page `rayline auth login` opens. |

An environment name is one or more ASCII letters, digits, `-` and `_`. The same rule applies to `--env`, to the names under `environments`, and to the environment slots in `credentials.json`. Any other name is an error that names it.

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
11. A model entry that is missing `context_window` or `max_output_tokens`. ARC knows no model today, so this applies to every entry (section 5.3 item 5).
12. An `api_key_env` naming `RAYLINE_ROUTER_API_KEY`, which holds the Rayline router key.

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
3. OS: `PATH`, and the home directory. One function reads it, from `HOME` on unix and `USERPROFILE` on Windows. When that var is unset or empty, it is an error that names the var. There is no other source for the home directory.
4. Harness homes, read to find a harness's own files: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCLAW_CONTAINER`.
5. `CLAUDE_CODE_DISABLE_AGENT_VIEW`, passed on to Claude Code unchanged. When it is `1`, `true`, `yes` or `on` (trimmed, any case, as Claude Code reads it), a cloud `rayline run claude` skips the reuse and conflict steps of the daemon check below: it never prompts or fails because a daemon is running. Rayline still reads `daemon.lock` and checks its `pid`, to keep the records of a running daemon's starter (last point below), so a failed liveness check is still an error.

   The daemon check, before each cloud `rayline run claude`:
   - A Claude Code daemon is running when `daemon.lock` in the launch's Claude config folder (`CLAUDE_CONFIG_DIR`, else `~/.claude`; with `--isolated`, `~/.rayline/harness/claude-code/`) names a `pid` that is alive. A missing or unreadable `daemon.lock`, or one that isn't JSON naming a positive `pid`, counts as no daemon running (Claude Code writes it non-atomically). When checking whether that process is alive fails, the launch fails with an error that names the pid.
   - Rayline reuses the running daemon when either:
     - its environment, where Rayline can read it (Linux and macOS), holds the launch's values of `RAYLINE_ENV_NAME`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK`, `CLAUDE_CODE_DISABLE_REFUSAL_RETRY`, `CLAUDE_CODE_TETHER_LIVE`, `CLAUDE_CODE_SUBAGENT_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL_FORCE` and `ANTHROPIC_DEFAULT_HAIKU_MODEL`. `ANTHROPIC_BASE_URL` is compared without trailing slashes. An empty value of one of the last six counts as unset, so an empty `ANTHROPIC_DEFAULT_HAIKU_MODEL` matches a launch that removes it; or
     - a record in `daemon-launches.json` (section 8.2) is for the process that started it (`spawnedBy.pid` in `daemon.lock`), with the same environment, router URL, auto-compact window and those switches, and the daemon started within 60 seconds after the record.
   - Any other running daemon conflicts. When stdin, stdout and stderr are all terminals, Rayline asks whether to stop it (`claude daemon stop --any`) and go on, to run isolated instead (offered only without `--isolated`; the isolated folder gets the same check), or to cancel. Rayline trims the answer and matches it ignoring ASCII case. An empty answer stops it: Enter alone, whitespace only, or end of input (such as Ctrl-D). So do `s`, `y` and `yes`. `i` or `isolated` runs isolated, when offered. Any other answer cancels. When not all three are terminals, on cancel, or when flushing the prompt to stderr or reading the answer fails, the launch fails with an error. Its first lines name the daemon's pid and say whether it is a Rayline daemon (with its environment), a daemon that isn't Rayline's, or one Rayline couldn't verify. It goes on:

     ```
     To switch:
       1. Stop the current daemon:   <stop>
       2. Re-run:                    rayline [--env <name> ]run claude [--isolated ]<args>
     ```

     `<args>` are the launch's Claude Code arguments, else `agents`, and `--env <name>` is there when the launch passed it. In the isolated folder, `<stop>` is `claude daemon stop --any (with CLAUDE_CONFIG_DIR=<folder>)`, naming the isolated folder, and the rerun keeps `--isolated`. Otherwise `<stop>` is `claude daemon stop --any`, the rerun has no `--isolated`, and the message ends with `Or run alongside it in an isolated config dir (~/.rayline/harness/claude-code):` and that rerun with `--isolated`.

     When the answer is to stop it, Rayline runs `claude daemon stop --any` with `CLAUDE_CONFIG_DIR` set to the checked folder, and goes on when it succeeds. When it exits non-zero, the launch fails with ``` `claude daemon stop --any` exited with code <code>. Stop the daemon manually and re-run. ```, where `<code>` is `-1` when the process has no exit code. When it can't be run, the launch fails with ``` Could not run `claude daemon stop --any`: <error>. Stop the daemon manually and re-run. ```
   - Each cloud launch adds its record to `daemon-launches.json`. It drops each existing record that has no `ts` or is 24 hours old or older, except that it keeps, whatever their age, every record whose `pid` is a `spawnedBy.pid` that a check read from a live daemon's `daemon.lock`, before that check's prompt. That is the check of the launch's config folder or, when the user chose isolated at the prompt, both the default folder's check and the isolated folder's, since the default folder's daemon keeps running. A record stays kept when the user then stops its daemon at the prompt. A missing `daemon-launches.json` holds no records, and recording the launch creates it. One that exists but can't be read, or isn't a JSON array of objects, is an error. So is failing to write it: creating its folder, writing the temporary file, or renaming that into place. Each of these fails the launch.
6. Shared Hugging Face cache: `HF_HOME`, `HF_HUB_CACHE`.

### 7.2 Set by Rayline for a harness

1. Claude Code: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`, `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK`, `CLAUDE_CODE_DISABLE_REFUSAL_RETRY`, `CLAUDE_CODE_TETHER_LIVE`, `RAYLINE_ENV_NAME`.
   - `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, in cloud mode: set to the router's auto-compact window for the model, or removed when the router gives none.
   - `CLAUDE_CODE_AUTO_MODE_SERVER`, in local mode: always `0`.
2. Other harnesses: `CODEX_HOME` (cloud-mode Codex and the Codex desktop app, set to Rayline's own folder for it, section 8.2), `HERMES_HOME`, `PI_CODING_AGENT_DIR`, `OPENCODE_CONFIG`, `OPENCLAW_CONFIG_PATH`.
3. Removed from the Claude Code environment: `ANTHROPIC_API_KEY`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`.
4. Removed from the `omp` environment: `CLAUDE_CONFIG_DIR`, `PI_PROFILE`, `OMP_PROFILE`.
5. Removed from every harness command Rayline starts: `RAYLINE_ROUTER_API_KEY`.
6. Removed in local mode: `RAYLINE_EXEC_AUTH_TOKEN`, and the env var each endpoint's `api_key_env` names, unless the harness's Rayline config sets that var to the local process's gateway token.

### 7.3 Internal: handoff between Rayline's own processes

Set by one Rayline process and read only by a child it starts. Never read from the user's shell:

1. `RAYLINE_EXEC_AUTH_TOKEN`, `RAYLINE_EXEC_PARENT_PIPE`, `RAYLINE_TOKEN_COUNTER_KEY`, `RAYLINE_HYBRID_GATEWAY_TOKEN`, `RAYLINE_ROUTER_API_KEY`.

### 7.4 Build time only

`RAYLINE_VERSION`. Not read at runtime.

### 7.5 Test builds only

`RAYLINE_TEST_DETACH_HELPER`. Compiled only into test builds, not into the shipped binary.

Every other env var Rayline reads today is gone (section 10).

## 8. Files

### 8.1 `~/.config/rayline`

Only two files:

1. `settings.json`: written by the user.
2. `credentials.json`: written by `rayline auth login` and `rayline auth logout`.

### 8.2 `~/.rayline`

```
~/.rayline/
  bin/                          rayline, rayline-core, rayline-core-LICENSE
  local/                        local process state
    core-config.json            config handed to the core
    core-config.counted.json    the same config with Rayline's token counter added
    gateway-token               secret shared by the CLI and the local process
    ready.json                  local process address once it is up
    supervisor.json             what is running
    runtime.lock                one local process at a time
    runtime.log                 its log
    harness/<name>/             files Rayline generates per harness in local mode:
                                pi, hermes, omp, opencode, openclaw
  harness/<name>/               files Rayline generates per harness in cloud mode: pi, hermes, omp,
                                opencode, openclaw, codex, codex-app,
                                claude-code (only with --isolated)
  harness/claude-code/daemon-launches.json
                                Claude Code daemons Rayline started
```

Each mode has its own harness folder because each writes its own endpoint, key and model list there.

Rayline writes nothing else under `~/.rayline` or `~/.config/rayline`. Outside them it writes only:
- the harness's own files named in 9.4;
- tokenizer files in the Hugging Face cache: `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `<home>/.cache/huggingface/hub`;
- with `rayline run codex configure` (9.4), `rayline.config.toml` in the user's Codex home (`CODEX_HOME`, else `~/.codex`, Codex's own default);
- with `rayline run claude --isolated` (9.3), in the user's Claude config folder (`CLAUDE_CONFIG_DIR`, else `~/.claude`): the folders `projects/`, `sessions/`, `plans/` and `paste-cache/` and an empty `history.jsonl`, each only if missing, which the isolated folder links to.

Tests use a temporary home folder.

## 9. Kept for review

These keep today's behaviour for now, under the new command shape. Their aliases and extra values are still removed (section 10.4).

1. `rayline auth login`: `-b, --no-browser` (device-code login) and `-p, --paste` (paste the callback URL). `--paste` with `--no-browser` behaves as today (`--paste` wins). Launching a harness while signed out signs in on the spot with the default flow when stdin and stdout are both terminals. Otherwise the launch fails as signed out. A local-mode launch that reuses a running local process doesn't read the credentials, so it runs even when signed out.
2. `rayline update`: `--check`, `--version <version>`, `--force`, `--dry-run`.
3. `rayline run claude --isolated`: gives Claude Code its own config folder (`~/.rayline/harness/claude-code/`) so it can run beside other sessions.
4. `rayline run codex`: `--model <model>`; the subcommands `rayline run codex app [WORKSPACE]` (launches the Codex desktop app on its own `CODEX_HOME`, `~/.rayline/harness/codex-app/`) and `rayline run codex configure` (writes a Codex profile, `rayline.config.toml`, to the user's Codex home: `CODEX_HOME`, else `~/.codex`, Codex's own default).

## 10. Removed

Everything below is deleted outright: code, tests, docs and help text.

### 10.1 Commands and options

1. `rayline router` and all its subcommands (`start`, `status`, `logs`, `top`, `stop`).
2. `rayline hybrid` and all its subcommands and options (`start`, `status`, `stop`, `run`, `--core`, `--config`, `--env`, `--local-context-window`, `--classifier-model`, `--cell`, `--port`, `--startup-timeout-secs`, `--state-dir`). `rayline local` and `rayline run --local` replace it.
3. Every subcommand of today's `rayline local`: `models`, `download`, `use`, `remove`, `custom`, `show`, `test`, `clear`, `on`, `off`, `onboard`, `subagents`, with `--url` and `--base-url`.
4. Top-level harness commands (`rayline claude`, `rayline codex`, `rayline claude-desktop`, `rayline pi`, ...) and the `rayline claude run` form. `rayline run <HARNESS>` replaces them.
5. `rayline claude` options: `-m, --model`, `--auto-compact-window`, `--local-provider`, `--route`, `--via`, `--local-injector-port`, `--statusline`, `--no-statusline`, `--diagnose`, `--upstream-ca-path`, `--router-config-path`, `--routing-mode`, `--no-proxy`, `--local-router`, `--no-telemetry`.
6. `--auth-token`, at the top level and on every command.
7. Top-level `rayline status`. `rayline auth status` is the only status command.
8. `top` options `--legacy`, `--hybrid`, `--all`, `--state-dir`, and the `a` key on the live screen.
9. `update --channel`. `update.channel` and `RAYLINE_UPDATE_CHANNEL` replace it.
10. The `rl` alias.
11. `rayline run claude-desktop` and all its commands and options (`start`, `status`, `validate`, `stop`, `install-service`, `uninstall-service`, `off`, `uninstall`, `--manual`, `--prefixed-ids`, `--desktop-config`), with its LaunchAgent and its state folder and lock. `rayline run claude-desktop` is an unknown harness.

### 10.2 The old local stack

1. The old local router, transparent proxy, injector, adapter and the `rld` binary with its `serve`, `proxy`, `statusline` and `models` commands. The local process gateway moves into `rayline` (section 4.3).
2. Proxy mode and its proxy CA (`HTTPS_PROXY` injection, `NODE_EXTRA_CA_CERTS`).
3. The on-device model manager: downloads, hardware fit, llama.cpp management, Ollama/LM Studio discovery, account `enable_local` switching, subagent discovery and routes.
4. claude-desktop's own router copy (its state folder, ports, pid file, LaunchAgent and `router.json`).
5. Workspace crates used only by the above.

### 10.3 Settings and files

1. Settings keys: `router_url`, `cli_auth_url`, `account_url`, `default_env`, `auth_kind`, `authKind`, `firebase_api_key`, `google_device_client_id`, `google_device_client_secret`, `local_model`, `hybrid` (all of it, including `local_registry_model`, `routing`, `local_tokenizer`, `routing.local_price`, `cloud_output_tokens`, `classifier_model`, `local_context_window`), `subagent_routes`, `onboarding`.
2. Files: `~/.config/rayline/cli.toml`, `router.json`, `router.<env>.json`, `claude-daemon-launches.json` and harness homes under `~/.config/rayline`; `~/.rayline/rld/`, `~/.rayline/hybrid/`, `~/.rayline/cc/`, `~/.rayline/codex-app-home/`, `~/.rayline/claude-desktop/`, `~/.rayline/claude-desktop.lock` and `~/.rayline/harness/claude-desktop/`. Rayline neither reads nor moves them.
3. The old settings file location and its migration.
4. Today's `credentials.json` shape (`version`, camelCase fields, `authKind`, `router_keys.<env>`, `apiKey`).

### 10.4 Aliases, extra values and guesses

1. Firebase login and everything behind `auth_kind`.
2. A missing `refresh_token` replaced by the access token.
3. Sign-in flow guessed from `SSH_CONNECTION`, `DISPLAY`, `WAYLAND_DISPLAY` or `BROWSER`. `--no-browser` is the explicit way.
4. API mode aliases (`claude`, `openai`, `responses`, `openai_responses`): one name per mode.
5. Codex `--auth`, with all its values (`auto`, `subscription`, `none`, `no-auth`, `local`, `codex-subscription`, `chatgpt`, `codex`), and `--subscription`.
6. `n_ctx` as a context-window alias.
7. "Legacy mode" for the `current`/`execution_context_v1` API modes, support for a core without the token-counter contract, and legacy adapter overrides.
8. The Decisions environment guessed from its URL.
9. A missing context window replaced by a default.
10. A failure to get the router key hidden as "no key".
11. A missing classifier key or model silently skipping the classifier route.
12. The model taken from `--model`, then the request, then `ANTHROPIC_MODEL`.
13. Lowercase proxy env var fallbacks.
14. Hard-coded model constants: `DECISIONS_MODEL`, `LOCAL_REGISTRY_MODEL`, `DEFAULT_LOCAL_CONTEXT_WINDOW`, `CLASSIFIER_MODEL`. `CLOUD_MODELS` and `CLOUD_CONTEXT_WINDOW` stay for now: in local mode, the models ARC can pick that are not in `models` are exactly the three Rayline cloud models in section 5.3 item 2, each with a 1,000,000-token window and a 128,000-token output ceiling. They go when the Rayline router publishes ARC's model set.
15. The second copy of ARC's preset list in the core.
16. Install leftovers: cleanup of the old uv install and handling for releases from before the core.

### 10.5 Env vars

1. Overrides of `credentials.json`: `RAYLINE_AUTH_TOKEN`, `RAYLINE_ID_TOKEN`, `RAYLINE_ROUTER_API_KEY` from the user's shell, `RAYLINE_KEY` and its scrubbing.
2. `CLAUDE_SETTINGS_PATH`, `RAYLINE_HARNESS_CONV_ID_FALLBACK`, `RAYLINE_CONV_ID`, `RAYLINE_PI_ALLOW_EMPTY_SIGNATURE`, `ANTHROPIC_MODEL` as a model source, `RAYLINE_CALLER_CWD`.
3. `RAYLINE_CLI_AUTH_URL`, `RAYLINE_MODELS_REGISTRY_URL`. `environments` covers them.
4. `RAYLINE_UPDATE_BASE_URL`, `RAYLINE_UPDATE_INSTALL_PATH`. Hidden `update` options replace them (section 4.2).
5. The old local router, `rld` and proxy mode: `RAYLINE_MODEL_REPO`, `RAYLINE_MODEL_FILE`, `RAYLINE_MODEL_REVISION`, `RAYLINE_MODEL_SHA256`, `RAYLINE_LLAMA_TAG`, `RAYLINE_CTX_SIZE`, `RAYLINE_LOCAL_MODEL_ID`, `RAYLINE_NO_LOCAL_MODEL`, `RAYLINE_ADAPTER_PORT`, `RAYLINE_ADAPTER_UPSTREAM_MODEL`, `RAYLINE_ADAPTER_UPSTREAM_URL`, `RAYLINE_INJECTOR_PORT`, `RAYLINE_PROXY_PORT`, `RAYLINE_ISOLATED_PROXY_PORT`, `RAYLINE_ROUTER_URL`, `RAYLINE_DECISION_PLANE`, `RAYLINE_LOCAL_ROUTER_PORT`, `RAYLINE_ROUTER_CONFIG`, `RAYLINE_DATA_DIR`, `RAYLINE_PROXY_CA_CERT_PATH`, `RAYLINE_PROXY_CA_KEY_PATH`, `RAYLINE_UPSTREAM_CA_FILE`, `RAYLINE_ROUTE_STATUS_PATH`, `RAYLINE_ANTHROPIC_URL`, `RAYLINE_PROXY_ROUTING_MODE`, `RAYLINE_CLAUDE_ROUTING_MODE`, `RAYLINE_METRICS_PORT`, `RAYLINE_METRICS_URL`, `RLD_BIN`; proxy injection of `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`, `NODE_EXTRA_CA_CERTS`; the old router config folder from `APPDATA` and `XDG_CONFIG_HOME`; `OLLAMA_HOST` and `LMSTUDIO_HOST`.
6. Installer overrides: `RAYLINE_INSTALL_DIR` (`--install-dir` replaces it), `RAYLINE_RELEASE_BASE_URL` (the installer's hidden `--base-url` replaces it, section 4.4) and `RAYLINE_MINISIGN_PUBKEY` (the installers trust only the pinned release key).

## 11. Done means tested

1. Unit tests cover every load error in section 5.8 and section 6, every usage error, and the env override table.
2. Every harness in section 3 runs in cloud mode and in local mode and completes a real turn, as it does today.
3. The default test suite needs no live credentials or external services. Live runs are documented commands, run before merge.
