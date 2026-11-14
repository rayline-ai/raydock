# Acceptance testing

`scripts/acceptance.sh` runs real turns through every harness, in cloud mode (`rayline run <harness>`) and in local mode (`rayline run <harness> --local`), for several models and four cases each. A change is done when every cell passes in both modes.

Cloud turns run on Rayline cloud and are billed to your account (some tens of cents per full run).

## Prerequisites

1. Rayline is installed (`rayline --version` works) and signed in (`rayline auth login`).
2. The harnesses you want to test are on `PATH`: `claude`, `codex`, `opencode`, `pi`, `omp`, `hermes`, `openclaw`. A harness that isn't installed fails; to test fewer, name them on the command line.
3. `~/.config/rayline/settings.json` (or the `CONFIG` file, below) has a `routing` section and the endpoints and models local mode uses. Every env var named by an `api_key_env` is set.
4. `bash`, `perl` and `python3` (all ship with macOS and most Linux distributions). `python3` runs the MCP tool server of the `heavy` case and makes session ids.

## settings.json

Local mode reads everything from `~/.config/rayline/settings.json`, or from the `CONFIG` file. The script never writes either. An example with one inference server:

```json
{
  "endpoints": {
    "gpu-box": {
      "base_url": "https://gpu-box.example.com:8443",
      "protocols": ["anthropic_messages", "openai_chat", "openai_responses"],
      "api_key_env": "GPU_BOX_API_KEY"
    }
  },
  "models": {
    "z-ai/glm-5.3-flash": {
      "endpoint": "gpu-box",
      "model": "GLM-5.3-Flash-EXL3",
      "context_window": 1000000,
      "max_output_tokens": 32000
    }
  },
  "routing": {
    "decisions_model": "arc-swift-1.0",
    "classifier_model": "anthropic/claude-haiku-5.5",
    "policy": { "preset": "arc-policy.prefer-local.v1" }
  }
}
```

The endpoint must list the protocol of every harness you run in local mode: `anthropic_messages` for `claude`, `pi`, `omp` and `opencode`; `openai_responses` for `codex` and `hermes`; `openai_chat` for `openclaw`. `docs/spec.md` section 5 describes every key.

## Run

```bash
scripts/acceptance.sh                                  # every harness, mode, model and case
scripts/acceptance.sh codex pi                         # only these harnesses
MODES=cloud scripts/acceptance.sh                      # one mode
MODELS="anthropic/claude-sonnet-5.5" CASES="tool multi" scripts/acceptance.sh
RAYLINE=target/release/rayline scripts/acceptance.sh   # test a local build
CONFIG=~/acceptance.json scripts/acceptance.sh         # local mode from this file
WORK=./acceptance-out scripts/acceptance.sh            # keep every turn's output here
```

| Variable | Default | Meaning |
|---|---|---|
| `MODES` | `cloud local` | Modes to run. |
| `MODELS` | `anthropic/claude-sonnet-5.5 openai/gpt-6.1-sol z-ai/glm-5.3-flash` | Model ids the harness asks for, one per provider family worth covering. |
| `CASES` | `plain tool heavy multi` | Cases to run (below). |
| `CONFIG` | | Settings file for local mode instead of `~/.config/rayline/settings.json`. |
| `TIMEOUT` | `600` | Seconds per turn. |
| `WORK` | a fresh temporary directory | Where each cell's working directory, output and `results.tsv` go. |
| `RAYLINE` | `rayline` | The CLI to test. |

`CONFIG=<file>` runs local mode from `<file>` instead of `~/.config/rayline/settings.json`, so you don't have to replace your own settings: the script runs `rayline local start --config <file>` and `rayline run <harness> --config <file>` (without `--local`, since a file with a `routing` section puts the run in local mode). Cloud mode still uses the default file. A `CONFIG` file without a `routing` section is an error before any turn runs.

The script:

1. Prints the Rayline version and each harness's version. Keep them with the results: a harness release can change how it talks to Rayline, so a result only holds for the versions it ran with.
2. Runs every cell in cloud mode: for each harness, each model, each case.
3. Starts the local process with `rayline local start`, runs every cell with `--local`, then stops it with `rayline local stop`. It also runs `rayline local stop` if it exits early, including on Ctrl-C. A failed `rayline local start` or `rayline local stop` is a `FAIL`, and so is a `rayline local status` that fails or doesn't report the process running. When `rayline local start` fails, the script stops nothing. Before any turn runs, it checks `rayline local status`: if a local process is already running, it prints `a local process is already running; stop it first` and exits 1, and if `rayline local status` fails, it prints its output and `rayline local status failed` and exits 1.

## Cases

Every cell has its own directory under `WORK`; the harness runs in its `work/` subdirectory, and the turn logs stay in the cell directory. Every turn is the harness's own non-interactive mode with the model and reasoning flags from the table below.

| Case | What the model must do | Catches |
|---|---|---|
| `plain` | Answer `ACCEPT` followed by 17 + 25. | The request path as such. |
| `tool` | Read `answer.txt` (two random integers) with a tool and answer `ACCEPT` followed by their sum, reasoning on. | A reasoning stream that aborts when a tool call follows it. |
| `heavy` | Call `lookup_record` from an MCP server (`scripts/acceptance/mcp_tools.py`) whose tool schemas carry top-level `oneOf`, `anyOf` and `allOf` with `$ref` branches, as the Codex Computer Use plugin's do, and answer `ACCEPT` followed by the value it returns. | A router or provider that refuses such schemas. |
| `multi` | Turn 1: read `answer.txt` and remember the two integers. Turn 2, in the same session and with the file removed: answer `ACCEPT` followed by their sum. | Reasoning or thinking that is not carried into the next turn. A harness that loses the session cannot recover the integers. |

The answer never appears in the prompt: the sum and the MCP value change every run, so an error that echoes the prompt can't pass.

| Harness | Turn | Model and reasoning | Session (`multi`) | MCP server (`heavy`) |
|---|---|---|---|---|
| claude | `-p "<prompt>"` with `MAX_THINKING_TOKENS=8000` | `--model <id>` | `--session-id <uuid>`, then `--resume <uuid>` | skipped: Claude Code drops tools with top-level `oneOf`/`anyOf`/`allOf` before any request |
| codex | `exec --skip-git-repo-check "<prompt>"` | `-m <id> -c model_reasoning_effort=high` | `session id:` from turn 1, then `exec ... resume <id>` | `-c mcp_servers.acceptance.command/args` |
| opencode | `run "<prompt>"` | `-m rayline/<id> --thinking`, models declared in a project `opencode.json` the script writes (no limits: the router and the local process enforce their own) | `-s ses_<id>` on both turns | skipped: OpenCode wraps MCP tools in its Code Mode `execute` tool, so their schemas never leave the harness |
| pi | `-p "<prompt>"` | `--model rayline/<id> --thinking high` | `--session-id <id>` on both turns | `.pi/mcp.json` in the working directory, `--approve` |
| omp | `-p "<prompt>"` | none: omp runs only the models in its catalog, so it runs once per case as `default` | `-c` on turn 2 | skipped: persistent config only |
| hermes | `chat -q "<prompt>"` | `-m <id> --reasoning high` (turn 1 only: Hermes refuses them on resume) | `Session:` from turn 1, then `--resume <id>` | skipped: persistent config only |
| openclaw | `agent -m "<prompt>"` | `--model rayline/<id> --thinking high` | `--session-id <id>` on both turns | skipped: persistent config only |

In local mode the local process decides which model runs, so the requested model is a request, not a pin. Each local cell line ends with `(executed <models>)`, the `executed_model` values the local process logged in `~/.rayline/local/runtime.log` during the turn.

Local mode requires a fresh harness session: `rayline run` refuses the session flags of `claude`, `codex`, `opencode` and `pi` before the harness starts, so their `multi` cells are `SKIP` in local mode, with the CLI's message as the reason. The `tool` case still covers reasoning carried into a later request in local mode: the request after the tool result holds the first request's reasoning.

## Results

Each cell prints one line:

- `PASS <harness> <mode> <model> <case>`: every turn exited 0 within `TIMEOUT` and the output contains the expected answer. A `NOTE` line follows when the output still mentions an error or a 4xx status.
- `FAIL ... : <cause>`: a turn exited non-zero, timed out, or never printed the answer. The cause is the last error line of the output when the turn exited non-zero, and the last 15 lines follow.
- `FAIL ... : not installed`: the harness isn't on `PATH`.
- `SKIP ... : <reason>`: the harness cannot run the cell (see the table above), or `rayline run` refused it by design. Skips don't fail the run.

`WORK/results.tsv` has one row per cell (`harness mode model case result executed cause`), and each cell's directory holds its turn output as `turn.log`, or `turn-1.log` and `turn-2.log`, next to the harness's `work/` directory. Router keys are redacted from the output. `RAYLINE`, `CONFIG` and `WORK` may be relative to the directory you run the script from.

The script exits 0 when nothing failed, 1 on any `FAIL`, when a local process is already running or when `rayline local status` fails before any turn, and 2 on a usage or config error.
