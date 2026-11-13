# Local mode

In local mode a coding harness talks to a Rayline process on your machine
instead of to Rayline cloud. For every request, ARC picks the model, and the
local process sends the request to one of your own inference servers listed in
`settings.json`, or to Rayline cloud for any other model.

Cloud mode, the default, needs none of this. See
[Getting started](getting-started.md).

## What you need

- Rayline, installed and signed in with `rayline auth login`.
- A config with a `routing` section: `~/.config/rayline/settings.json`, or a
  file you pass with `--config`.

Your own inference servers are optional. With none listed, every model ARC
picks goes to Rayline cloud. Rayline doesn't start, download or stop your
servers, and `rayline local stop` leaves them running.

ARC makes every routing decision. See
[ARC pricing](https://docs.rayline.ai/arc/pricing) for how sessions are billed.

## Configure

The config has three sections. A local-mode config needs `routing`. Add
`endpoints` and `models` for each model your own servers serve.

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

Rayline cloud is built in and has no entry. It serves every protocol a harness
speaks.

### `endpoints`

These are your own inference servers, keyed by an id you choose.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `base_url` | string (URL) | yes | The server's address. |
| `protocols` | list of `anthropic_messages`, `openai_chat`, `openai_responses` | yes, non-empty | The wire protocols it serves. |
| `api_key_env` | string | no | The name of the env var that holds the server's key. Never put the key itself in the file. |

Each harness speaks one protocol:

| Harness | Protocol |
|---|---|
| `claude`, `claude-desktop` | `anthropic_messages` |
| `codex` | `openai_responses` |
| `opencode`, `pi`, `omp`, `hermes`, `openclaw` | `openai_chat` |

Requests pass straight through to an endpoint, with no translation. In a
local-mode run, every endpoint in the config must list the protocol of the
harness you run, or Rayline refuses to run. With the example above, `claude`
and `pi` can run in local mode, but `codex` can't, because `ollama` doesn't
list `openai_responses`.

### `models`

These are the models ARC can pick that Rayline cloud doesn't serve. Each is
keyed by the model id ARC returns.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `endpoint` | string | yes | An id in `endpoints`. |
| `model` | string | yes | The name the server knows the model by. |
| `context_window` | integer > 0 | yes if ARC doesn't know the model | The context window in tokens. |
| `max_output_tokens` | integer > 0 | yes if ARC doesn't know the model | The output ceiling in tokens. |
| `tokenizer` | string | no | A Hugging Face tokenizer repo, used to count tokens. |
| `price` | object with `input`, `output`, `cache_read`, `cache_write` (numbers >= 0) | no | Replaces ARC's price for this model only. |

When ARC picks a model listed here, the local process sends the request to
that model's endpoint under its `model` name. Any other model ARC picks goes
to Rayline cloud under the id ARC returned, and its limits and price come
from ARC. Every entry works the same way. No setting marks a model as local.

### `routing`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `decisions_model` | string | yes | The ARC product that makes the decisions, for example `arc-swift-1.0`. |
| `classifier_model` | string | yes | The model that serves Claude Code's auto-mode safety classifier calls. These calls go to Rayline cloud. |
| `policy.preset` | non-empty string | yes | The id of an ARC preset: `arc-policy.package-default.v1`, `arc-policy.hybrid-default.v1` or `arc-policy.prefer-local.v1`. It is sent to ARC unchanged. |
| `policy.overrides` | object | no | Overrides in ARC's own `routing.policy.overrides` shape, sent to ARC unchanged. ARC validates them. |

The [ARC routing policies](https://docs.rayline.ai/arc/routes/hybrid)
page describes how ARC applies each preset and which overrides it accepts.

### Env overrides

Three `routing` keys can be set from the environment:

| Key | Env var |
|---|---|
| `routing.decisions_model` | `RAYLINE_ROUTING_DECISIONS_MODEL` |
| `routing.classifier_model` | `RAYLINE_ROUTING_CLASSIFIER_MODEL` |
| `routing.policy.preset` | `RAYLINE_ROUTING_POLICY_PRESET` |

- From lowest to highest precedence: built-in defaults, the config file, env
  overrides, then flags.
- An override is validated the same way as the file value.
- An override only changes a value. The config file still has to have a
  `routing` section, because the file decides the mode.
- `endpoints`, `models` and `policy.overrides` can't be set from the
  environment. Put them in a file and pass it with `--config`.

## Start and run

```bash
rayline local start
rayline run claude --local
rayline run pi --local -- "Fix the failing test in parser.py"
rayline local status
rayline local stop
```

| Command | Does |
|---|---|
| `rayline local start [--config <file>]` | Starts the local process from the default config file, or from `<file>`. |
| `rayline local status` | Shows whether the local process is running, its address and the config file it was started from. |
| `rayline local stop` | Stops the local process. Your inference servers keep running. |
| `rayline run <HARNESS> --local` | Runs the harness in local mode with `~/.config/rayline/settings.json`. Starts the local process if it isn't running. |
| `rayline run <HARNESS> --config <file>` | Uses `<file>` instead of the default config, never merged with it. The run is in local mode if `<file>` has a `routing` section, and in cloud mode if it doesn't. |

- `--local` and `--config` together are a usage error.
- Only one local process runs per machine.
- Running `rayline local start` while the process is already running is an
  error. The error names the running process and its config file.
- A local-mode run whose config file differs from the running process's file
  is an error. To use another file, run `rayline local stop`, then start again
  with that file.

## Watch requests

```bash
rayline top
rayline top --json
```

`rayline top` shows live request metrics from the local process. `--json`
prints one snapshot and exits. If no local process is running, `rayline top`
says so and exits with an error.

## Config errors

Rayline checks the config once, when it loads, and refuses to run with an
error that names the key. It refuses:

1. An unknown key, at any level Rayline defines.
2. A wrong type, an empty string, or a value out of range.
3. In a local-mode run, an endpoint that doesn't list the protocol of the
   harness being run. Cloud mode doesn't use endpoints, and
   `rayline local start` runs no harness, so neither checks this.
4. A model whose `endpoint` isn't in `endpoints`.
5. Local mode without `routing`, or a `routing` section that is missing a
   required key.
6. A routing env override when the config file has no `routing` section.
7. A `--config <file>` that doesn't exist. Only the default file is optional.
8. A key repeated in one JSON object, an empty map key, a URL that isn't an
   absolute `http` or `https` URL, a protocol listed twice in `protocols`, or a
   `price` that is missing one of its four fields.

Before the local process serves anything, it also refuses:

9. An `api_key_env` that names an env var that isn't set.
10. A model ARC doesn't know whose entry is missing `context_window` or
    `max_output_tokens`.

## Files

The local process keeps its state in `~/.rayline/local/`:

| File | Holds |
|---|---|
| `core-config.json` | The config handed to the execution core. |
| `gateway-token` | The secret the CLI and the local process share. |
| `ready.json` | The local process's address, once it is up. |
| `supervisor.json` | What is running. |
| `runtime.lock` | The lock that keeps it to one local process. |
| `runtime.log` | The local process's log. |

If a local-mode run fails, check `rayline local status` and
`~/.rayline/local/runtime.log` first.
