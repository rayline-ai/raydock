# Acceptance testing

`scripts/acceptance.sh` runs one real turn through every installed harness, in cloud mode (`rayline run <harness>`) and in local mode (`rayline run <harness> --local`). A change is done when every harness passes in both modes.

Cloud turns run on Rayline cloud and are billed to your account (a few cents per run).

## Prerequisites

1. Rayline is installed (`rayline --version` works) and signed in (`rayline auth login`).
2. The harnesses you want to test are on `PATH`: `claude`, `codex`, `opencode`, `pi`, `omp`, `hermes`, `openclaw`. Claude Desktop is tested by hand (below). The script skips any harness that isn't installed.
3. `~/.config/rayline/settings.json` has a `routing` section and the endpoints and models local mode uses. Every env var named by an `api_key_env` is set.
4. `bash` and `perl` (both ship with macOS and most Linux distributions).

## settings.json

Local mode reads everything from `~/.config/rayline/settings.json`. The script never writes it. An example with one inference server:

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

The endpoint must list the protocol of every harness you run in local mode: `anthropic_messages` for `claude` and `claude-desktop`, `openai_responses` for `codex` and `openai_chat` for the others. `docs/spec.md` section 5 describes every key.

## Run

```bash
scripts/acceptance.sh              # every harness
scripts/acceptance.sh codex pi     # only these harnesses
RAYLINE=target/release/raydock scripts/acceptance.sh   # test a local build
```

The script:

1. Prints the Rayline version and each harness's version. Keep them with the results: a harness release can change how it talks to Rayline, so a result only holds for the versions it ran with.
2. Runs one turn per harness in cloud mode.
3. Starts the local process with `rayline local start`, runs one turn per harness with `--local`, then stops it with `rayline local stop`. If a local process is already running, `rayline local start` refuses and local mode fails; stop it first.

Each turn is the harness's own non-interactive mode:

| Harness | Command |
|---|---|
| claude | `rayline run claude [--local] -- -p "<prompt>"` |
| codex | `rayline run codex [--local] -- exec --skip-git-repo-check "<prompt>"` |
| opencode | `rayline run opencode [--local] -- run "<prompt>"` |
| pi | `rayline run pi [--local] -- -p "<prompt>"` |
| omp | `rayline run omp [--local] -- -p "<prompt>"` |
| hermes | `rayline run hermes [--local] -- chat -Q -q "<prompt>"` |
| openclaw | `rayline run openclaw [--local] -- agent -m "<prompt>"` |

## Results

Each run prints one line:

- `PASS`: the harness exited 0 within 10 minutes and its output contains `ACCEPT42`. The prompt asks for `ACCEPT` followed by 17 + 25, so the answer never appears in the prompt, and an error that echoes the prompt can't pass.
- `FAIL`: the harness exited non-zero, timed out, or never printed the answer. The exit status and the last 15 lines of output follow.
- `SKIP`: the harness isn't installed.
- `MANUAL`: Claude Desktop, which needs the check below.

The script exits 0 when nothing failed, 1 when any run failed, and 2 on a usage error.

A pass shows that the turn completed. It doesn't show which model served it. In local mode, `rayline top` and `~/.rayline/local/runtime.log` show each decision and the model that ran.

## Claude Desktop

Claude Desktop has no non-interactive mode, so it is checked by hand in each mode:

1. Run `rayline run claude-desktop` (cloud) or `rayline run claude-desktop --local` (local).
2. Confirm Desktop opens with the Rayline gateway selected.
3. Open **Code**, pick an ARC model and send the prompt from the script. Confirm the reply is `ACCEPT42`.
4. Run `rayline run claude-desktop off` and confirm Desktop's previous profile and mode are back.
