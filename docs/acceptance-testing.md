# Acceptance Testing

This guide is for maintainers and coding agents who need to validate the real
Claude Code path through Rayline Local end to end — exercising it the way a user
would, against the local router and managed llama.cpp runtime. It complements the
automated gate (`fmt` / `clippy` / `test`); run these manual steps on a real
machine with a TTY after that gate is green.

Prerequisites:

- Claude Code is installed and already logged in.
- Rust `1.88.0` is installed when testing from source.
- `jq` is available for JSON inspection.

## Install Matching Binaries

From a Rayline Local source checkout:

```bash
cargo build --release -p rayline-cli -p rayline-daemon --locked

# macOS only: use a persistent identity so Keychain approval survives rebuilds.
RAYLINE_CODESIGN_IDENTITY='Apple Development: Your Name (TEAMID)' \
  scripts/sign-macos-rld.sh target/release/rld

install -m 0755 target/release/rayline "$HOME/.rayline/bin/rayline"
install -m 0755 target/release/rld "$HOME/.rayline/bin/rld"
export PATH="$HOME/.rayline/bin:$PATH"

rayline --version
rld --version
```

On Linux, omit the signing command. Avoid `cargo install` for macOS acceptance
testing: it installs an ad-hoc binary whose Keychain authorization is tied to
that one build hash.

The two versions should match. If you are testing an already installed release,
skip this step and use the installed `rayline`.

## Select a Local Model

Use a managed recommended model:

```bash
rayline local models
rayline local use qwen3.6-35b-a3b-q4km
rayline local show
```

First use may download the model. First launch may take a few minutes while
llama.cpp loads it.

## Configure Explore Routing

Create a config that routes only the `Explore` subagent to the local model:

```bash
mkdir -p ~/.config/rayline

cat > ~/.config/rayline/local-router-explore-local.json <<'JSON'
{
  "routes": {
    "subagents": {
      "Explore": { "endpoint": "local" },
      "explore": { "endpoint": "local" }
    }
  }
}
JSON
```

## Clear Stale Test State

Stop any previous isolated Claude Code daemon and Rayline Local router:

```bash
CLAUDE_CONFIG_DIR="$HOME/.rayline/cc" claude daemon stop --any || true
rayline router stop || true
```

## Run Claude Through Rayline Local

Run interactively. On the first `--isolated` run the profile is logged out, so a
browser sign-in opens automatically — complete it and the session continues.
(Print mode `-p` can't drive the browser, so sign in interactively first.)

<a href="https://get.rayline.ai/media/rayline-login-demo.mp4">
  <img src="https://get.rayline.ai/media/rayline-login-demo.gif"
       alt="rayline claude --local --isolated: browser sign-in, then an Explore subagent routed to the on-device model"
       width="100%">
</a>

```bash
rayline claude \
  --local \
  --isolated \
  --route subagents \
  --router-config-path ~/.config/rayline/local-router-explore-local.json
```

At the prompt, paste: `Use the Explore subagent for this task. Ask the Explore
subagent to reply exactly: RAYLINE_EXPLORE_ACCEPTANCE_OK. After the Explore
subagent returns, reply exactly: RAYLINE_EXPLORE_ACCEPTANCE_OK.`

Expected reply:

```text
RAYLINE_EXPLORE_ACCEPTANCE_OK
```

## Verify Routing

Check the router:

```bash
rayline router status
rayline router top --json \
  | jq '.active[]?, .recent[]? | select(((.agent_type // "") | ascii_downcase) == "explore" and .target == "local")'
```

A passing row shows `agent_type` as `Explore`, `target` as `local`, and a local
selected model.

Check logs:

```bash
tail -n 160 ~/.rayline/rld/rl-rld.log \
  | grep -i 'policy=subagent:Explore'

tail -n 160 ~/.rayline/rld/cc/rl-rld-proxy.log \
  | grep -i 'selective_subagent_header'
```

The proxy log should include:

```text
target=Router reason=selective_subagent_header
```

---

## `--local` Onboarding Acceptance

These steps require a real machine with a TTY (no headless runner). Run them after
the automated gate (`fmt` / `clippy` / `test`) is green.

### Prerequisites

```bash
# Confirm the binary is fresh (should match current branch build)
rayline --version
```

### Step 2: First-run onboarding (interactive wizard)

```bash
# Reset to a clean slate
rayline local clear
# Also remove the "onboarding" marker so the first-run wizard re-triggers.
# (portable; macOS has no `sponge`)
tmp=$(mktemp) && jq 'del(.onboarding)' ~/.config/rayline/settings.json > "$tmp" && mv "$tmp" ~/.config/rayline/settings.json

rayline claude --local
# (Tip: `rayline local onboard` re-runs the wizard directly, no settings edit needed.)
```

**Expected:**
- The wizard prints the recommendation headline + 4 choices: `[Y] download & use recommended (default) / [m] see all models / [o] use my own server (Ollama / LM Studio / llama.cpp) / [s] skip — stay on cloud`
- Choosing `[Y]` downloads and selects the recommended model
- `cat ~/.config/rayline/settings.json | jq .local_model` shows a `local_model` block (endpoint, model_id)
- `cat ~/.config/rayline/settings.json | jq .onboarding` shows `schema: 1`, `outcome: "local-model"`, a `completed_at` timestamp, and `cli_version`
- After the wizard, Claude routes only `Explore`/`codebase-*` subagents locally; main thread and other agents stay cloud
- Verify: `rayline router top` shows `policy=subagent:Explore target=local` rows; main rows stay `target=cloud`
- Or: `grep 'policy=subagent:Explore' ~/.rayline/rld/rl-rld.log`

### Step 3: Re-run + `--reset`

```bash
rayline local onboard            # wizard re-runs; keeps local_model unless changed
rayline local onboard --reset    # clears local_model first, then wizard
```

**Expected:**
- After `rayline local onboard`: `.onboarding.completed_at` in settings.json is updated to a newer timestamp
- After `rayline local onboard --reset`: `.local_model` is cleared before the wizard starts, then re-filled on `[Y]`

### Step 4: Non-interactive fallback

```bash
rayline local clear
echo "" | rayline claude --local -p 'hello'
```

**Expected:**
- No wizard prompt appears
- A clear "No local model configured…" (or similar) message is printed
- Non-zero exit code (`echo $?` should be non-zero)
- `cat ~/.config/rayline/settings.json | jq .onboarding` is `null` (no marker written)

### Step 5: Skipped → re-prompt

```bash
rayline local clear
rayline claude --local        # when prompted, choose [s] (skip) → outcome "skipped"
rayline claude --local        # MUST re-prompt even though a skip marker was recorded
```

**Expected:**
- First run writes `.onboarding.outcome = "skipped"` to settings.json
- Second run (with explicit `--local`) shows the wizard again — skip is not sticky for explicit `--local`

### Step 6: `--route all` widens past the allowlist

```bash
rayline local clear
rayline local use <some-model>   # configure a model without going through onboarding
rayline claude --local --route all
```

**Expected:**
- All subagents (not just the read-only allowlist) route locally
- Main thread stays cloud
- The materialized `local-default-routes.json` (written at launch under `~/.rayline/rld/`) is NOT applied — `--route all` (Proxy mode) bypasses it
- Verify via `rayline router top`: subagent rows show `target=local`, main rows show `target=cloud`

### Step 7: Local provider endpoint

With Ollama running and at least one model installed:

```bash
ollama serve
ollama list
rayline claude --local --local-provider ollama --model <ollama-tag> -p 'Use the Explore subagent to list the top-level files in this repo.'
```

**Expected:**
- `rayline local show` reports `Local model: Ollama`, the provider URL, the selected model, and `Protocol: openai_chat`
- `cat ~/.config/rayline/settings.json | jq .local_model` includes `"mode": "custom"`, `"provider": "ollama"`, `"protocol": "openai_chat"`, root `"base_url": "http://localhost:11434"`, and the selected model
- `~/.rayline/rld/provider-routes.json` contains an `openai_chat` endpoint named `ollama` plus `routes.subagents`, `routes.subagent`, and `routes.model_routes` entries pointing to that endpoint
- `~/.rayline/rld/rl-rld.log` shows custom upstream mode and does **not** show GGUF download or bundled `llama-server spawned`
- `rayline router top` shows Explore/subagent rows targeting the provider endpoint; main rows stay cloud

Stopped-provider failure:

```bash
rayline router stop
# stop ollama / LM Studio
rayline claude --local --local-provider ollama --model <ollama-tag> -p 'hello'
```

**Expected:**
- The command exits non-zero
- The error names the provider URL and prints the start hint (`ollama serve`)
- It does not silently fall back to cloud routing

Auto-discovery picker:

```bash
ollama serve
rayline local models
rayline local use <provider-row-number>
rayline claude --local -p 'hello'
```

**Expected:**
- `rayline local models` lists detected provider models before built-in models
- Selecting a provider row configures it immediately (no GGUF download)
- A later bare `rayline claude --local` reuses the persisted provider config and regenerates provider routes

---

## Manual CLI Checklist

A broader sweep of the CLI surface, beyond the routing path above. The routing
steps (3–5) reuse the same setup as
[Run Claude Through Rayline Local](#run-claude-through-rayline-local); verify
routing there rather than duplicating those assertions here.

1. **Version and help:**

   ```bash
   rayline --version
   rayline --help
   rayline claude --help
   rayline update --help
   ```

   Confirm the CLI exposes `claude`, `router`, `local`, and `update`, and that
   `rayline claude --help` describes local routing and includes `--local`,
   `--isolated`, `--via`, `--route`, and `--router-config-path`.

2. **Update check:**

   ```bash
   rayline update --check
   ```

   Confirm the command reports either `Already on latest` (exit code 0) or
   `Update available` (exit code 1). For install-path validation without
   replacing the binary, run `rayline update --dry-run --version <version>`.

3. **Isolated launch:**

   ```bash
   rayline claude \
     --local \
     --isolated \
     --router-config-path ~/.config/rayline/local-router.json
   ```

   Confirm `~/.rayline/cc/settings.json` exists and
   `~/.rayline/rld/cc/rl-rld-proxy.log` is written.

4. **Default route (main-thread passthrough):** In the launched session, run a
   normal main-thread prompt. Confirm the isolated proxy log shows passthrough
   routing (for example `selective_main_passthrough`) and the local-router log
   does not show that prompt as an `Explore` local route.

5. **`Explore` route:** Ask Claude Code to use the `Explore` subagent. Confirm
   the proxy log shows `selective_subagent_header` and the router log shows a
   local route with `policy=subagent:Explore` (or another case-insensitive
   `Explore` match).

6. **Log inspection:**

   ```bash
   rayline router status
   rayline router logs --lines 120
   rayline router top
   tail -n 120 ~/.rayline/rld/rl-rld.log
   tail -n 120 ~/.rayline/rld/cc/rl-rld-proxy.log
   ```

   Confirm the status reports the expected ports and the logs identify the router
   config path, selected endpoint, and local model.

7. **Parallel isolated sessions:** Keep a normal Claude Code session running, then
   launch the isolated local-router command in another terminal. Confirm the
   normal session keeps using its own daemon and the isolated session writes only
   under `~/.rayline/cc` and `~/.rayline/rld/cc` for Claude/proxy state. Start a
   second isolated launch with the same config and confirm it reuses or restarts
   only the isolated proxy, not the shared normal-session proxy.

---

## Troubleshooting

If Claude reports a non-Rayline Local daemon conflict, stop only the isolated
daemon and retry:

```bash
CLAUDE_CONFIG_DIR="$HOME/.rayline/cc" claude daemon stop --any || true
```

If `Explore` does not route locally, confirm the command used
`--route subagents` (the default for `--local`) and `--router-config-path`.
Main-thread Claude traffic should remain passthrough in this mode.

If the model is slow or missing, watch:

```bash
tail -f ~/.rayline/rld/rl-rld.log
tail -f ~/.rayline/rld/llama-server.log
```
