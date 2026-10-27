# Routing-mode verification runbook (for an agent or a human)

This is a self-contained runbook for **you (the agent) to execute**: run each
supported `--config` routing mode end-to-end with `visual-test.sh`, **one at a
time**, observe the routing, judge it against the criteria here, and write a report.
Run modes individually — do **not** wrap them in one long script: local-main modes
hang, and a per-mode loop lets you recover and continue.

> Honest scope: the *config-driven* routing (which endpoint each class hits, and the
> pinned/local models) is deterministic and verifiable. The *hosted* parts — the
> cloud router's model pick, and the may-local redirect — are **non-deterministic
> and account-gated**, so those rows are **REVIEW** (record what happened, judge by
> the rules below), not mechanical PASS/FAIL.

---

## 1. Prerequisites (verify all; if one is missing, note it — do not work around it)

| # | Requirement | Check | If missing |
|---|---|---|---|
| 1 | `raydock`/`rld` built from the branch under test, installed, **all stale `rld` killed** | `raydock --version`; `pgrep -fl "rld serve\|rld proxy"` → none | rebuild + install (Apple Silicon: `codesign --force --sign -` after copying); `pkill -9 -f "rld serve\|rld proxy"` |
| 2 | Signed in to rayline | `raydock auth status` (or `raydock local show` works) | `raydock auth login` |
| 3 | `ANTHROPIC_API_KEY` exported | `[ -n "$ANTHROPIC_API_KEY" ]` | Rc-K/Rl-K/L-K subagent leg will **FAIL** — mark those rows accordingly, don't hide it |
| 3b | `RAYLINE_ROUTER_API_KEY` is an **`rlk-`** key, for the Router column | `case $RAYLINE_ROUTER_API_KEY in rlk-*) ;; *) echo wrong;; esac` | cloud subagent legs 401 (unset) or **403** (session token) — see README ⁷. `raydock auth token` returns an `rls_` session token and is **not** usable here |
| 4 | `claude` (Claude Code) on PATH + Claude **subscription** logged in | `command -v claude` | passthrough modes (S-Rc/S-Rcl/S-Rl/S-L main) can't run |
| 5 | ollama up with both models | `curl -s localhost:11434/api/tags` shows `qwen3.5:9b` + `qwen2.5-coder:7b` | `ollama pull qwen3.5:9b && ollama pull qwen2.5-coder:7b` |
| 6 | local model configured + ON | `raydock local show` → "Custom endpoint … qwen2.5-coder:7b" + "account: ON" | `raydock local custom --url http://127.0.0.1:11434 --model qwen2.5-coder:7b`; the script runs `raydock local on` |
| 7 | `python3` available | `python3 --version` | install python3 |
| 8 | ports 20809–20813 free | script handles via killing `rld` | — |

asciinema/tmux are **not** required (the script runs the demo headless).

---

## 2. Procedure — run each mode yourself, sequentially

Set once (the prompt forces a main turn + an `Explore` per-type subagent + a
`general-purpose` default subagent, so every routing slot is exercised):

```bash
RB="$(command -v rayline)"   # or the absolute path to the branch-under-test binary
PROMPT='Launch TWO subagents with the Task tool, in parallel: (1) an Explore subagent and (2) a general-purpose subagent. Each must reply with exactly the word PONG. Do not use any other tool first. After both return, output DONE.'
```

Then, **for each MODE in this order** — `Rc-Rc Rc-K Rc-L Rcl-Rcl Rl-Rl Rl-K Rc-L-per-type S-Rc
S-Rcl S-L L-Rc L-Rl L-K L-L Rl-L S-Rl` (skip `Rcl-K Rcl-L L-Rcl`, see §5) — do exactly this:

```bash
# a) clean slate so `raydock top` reflects only this mode (each run restarts rld)
pkill -9 -f "rld serve|rld proxy"; sleep 1
# b) run the mode's demo headless, with a ~100s timeout (local-main modes are slow —
#    a pinned-window 9B took ~6 min to reach the spawn; raise the bound for L-* modes)
DEMO_HEADLESS=1 RAYLINE_BIN="$RB" ./examples/routing-modes/visual-test.sh "$MODE" "$PROMPT" >/tmp/v-$MODE.out 2>&1 &
P=$!; for i in $(seq 1 20); do sleep 5; kill -0 $P 2>/dev/null || break; done; kill -9 $P 2>/dev/null
# c) read what actually routed (do NOT trust the demo's own summary if it was killed):
"$RB" top --json --all     # per request: agent_type, target, selected_model, policy
grep -i "local route endpoint" ~/.rayline/rld/rl-rld.log | tail -8   # endpoint-level detail for LSR modes
grep -i "DONE" /tmp/v-$MODE.out >/dev/null && echo "run finished" || echo "run did NOT finish (expected for local-main)"
```

Judge that mode against §3 (signals) + §4 (criteria), record one line per §6, then
move to the next mode. Don't batch them; one at a time.

---

## 3. How to read `target` / `policy` (the signals)

- `target=anthropic`, `policy=selective_main_passthrough` → **Claude subscription passthrough** (main of S-Rc/S-Rcl/S-Rl/S-L).
- `target=local` or `policy` contains `local-adapter` / `:may-local` → **served by a local model** (the may-local redirect, or the LSR sending it local).
- `target=remote` → an upstream endpoint; use **`selected_model`** to tell which:
  - `qwen3.5:9b` / `qwen2.5-coder:7b` → **local (ollama)** routed by the LSR.
  - `z-ai/glm-5.2`, `deepseek/deepseek-v4-pro` → **cloud, pinned by the JSON** (LSR rewrote the model).
  - `claude-*`, `deepseek/deepseek-v4-flash`, etc. with no pin → **cloud RCR's own pick** (non-deterministic).
  - `claude-sonnet-5` on a **subagent** in Rc-K/Rl-K → the **anthropic API-key endpoint** (distinguish from an RCR pick by the mode's intent).

---

## 4. Per-mode criteria

`S/K/R/L` = provider class per leg (main/subagent); "cloud(pick)" = RCR-chosen model (any cloud model, REVIEW).
"DONE finished" in the report means the run completed; local-main runs often won't.

| Mode | Expected main → | Expected subagents → | PASS when | Class |
|---|---|---|---|---|
| **Rc-Rc** | cloud (RCR pick) | cloud (RCR pick); Explore may→local (local ON) | main `target=remote`; subagents remote, OR Explore local (may-local) | REVIEW (cloud pick + may-local) |
| **Rc-K** | cloud (RCR pick) | anthropic (API key) | main remote; subagent `claude-sonnet-5` via anthropic | FAIL if `ANTHROPIC_API_KEY` unset |
| **Rc-L** | cloud (RCR pick) | **ollama** `qwen3.5:9b` | main remote; subagents `selected_model=qwen3.5:9b` | PASS (deterministic subagent) |
| **Rcl-Rcl** § | cloud (RCR pick) | cloud + may-local | same as Rc-Rc; behaviorally ≡ Rc-Rc (may-local account-gated) | REVIEW |
| **Rl-Rl** | cloud **pinned `z-ai/glm-5.2`** | cloud **pinned `deepseek/deepseek-v4-pro`** | main `selected_model=z-ai/glm-5.2`; subagents `deepseek/deepseek-v4-pro` | **PASS (deterministic, exact models)** |
| **Rl-K** | cloud **pinned `z-ai/glm-5.2`** | anthropic (API key) | main `z-ai/glm-5.2`; subagent anthropic `claude-sonnet-5` | main PASS; subagent FAIL if no key |
| **Rc-L-per-type** | cloud (RCR pick) | `Explore`→ollama `qwen2.5-coder:7b`, `Plan`→ollama `qwen3.5:9b`, other→cloud | Explore `qwen2.5-coder:7b`; general-purpose remote (cloud) | **PASS (deterministic per-type)** |
| **S-Rc** | **subscription** (passthrough) | cloud (RCR pick) | main `target=anthropic`/`selective_main_passthrough`; subagents remote | REVIEW (cloud pick) |
| **S-Rcl** § | subscription | cloud + may-local | main passthrough; subagents remote or Explore→local | REVIEW |
| **S-L** | subscription | **ollama** `qwen…` | main passthrough; subagents `qwen*` (local) | **PASS** (passthrough + local) |
| **L-Rc** ‡ | **ollama** `qwen3.5:9b` | cloud (RCR pick) | main `selected_model=qwen3.5:9b`; subagents remote | REVIEW (local main; pin the window first) |
| **L-Rl** ‡ | ollama `qwen3.5:9b` | cloud pinned `deepseek/deepseek-v4-pro` | main `qwen3.5:9b`; subagents `deepseek/deepseek-v4-pro` | REVIEW (local main; the pin can 403) |
| **L-K** ‡ | ollama `qwen3.5:9b` | anthropic (API key) | main `qwen3.5:9b`; subagents `claude-sonnet-5` | REVIEW (local main + needs key) |
| **L-L** ‡ | ollama `qwen3.5:9b` | **ollama** | main `qwen3.5:9b`; subagents `qwen3.5:9b` | REVIEW (local main) |
| **Rl-L** | cloud **pinned `z-ai/glm-5.2`** | **ollama** `qwen2.5-coder:7b` | main `z-ai/glm-5.2`; subagents `qwen2.5-coder:7b` | **PASS (deterministic)** |
| **S-Rl** | subscription | cloud **pinned `deepseek/deepseek-v4-pro`** | main passthrough; subagents `deepseek/deepseek-v4-pro` | **PASS (deterministic subagent)** |

**§ may-local (Rcl-Rcl/S-Rcl):** the redirect is the hosted RCR's **account-gated**
decision; with local ON it usually sends `Explore` to local, but it's discretionary.
Because the implicit account-local path also advertises, Rc-Rc/S-Rc behave the same —
so Rcl-Rcl/S-Rcl are **not** distinguishable from Rc-Rc/S-Rc by routing alone. Mark REVIEW.

**‡ local-main (L-Rc/L-Rl/L-K/L-L):** the main runs on a small local model. **Pin its
context window first** — ollama's VRAM-derived default (often 4096; check `ollama ps`,
CONTEXT column) truncates the tool schemas out of the prompt, and the model then
narrates tool calls as prose instead of invoking them. Bake it into a tag (`FROM
qwen3.5:9b` + `PARAMETER num_ctx 32768`, `ollama create …-32k -f Modelfile`, then name
that tag in the config's `models` and `routes.*.model`) or raise
`OLLAMA_CONTEXT_LENGTH`; ollama drops `options.num_ctx` on the compat routes Rayline
speaks, so Rayline cannot inject it. See the README's footnote ¹.

With the window pinned the local main **does** drive Claude Code's Task tool and
spawns subagents — verify both the **main** route to ollama and the `task=subagent`
lines. Spawning is still **flaky** (~1 in 4 attempts the main narrates the answer and
stops), so a single no-spawn run is not a failure; re-run before scoring. For the
**Router** entry point, `raydock router start` does **not** inject any credential —
export an **`rlk-` router key** as `RAYLINE_ROUTER_API_KEY` first, or cloud subagent
legs fail while the local main still prints a plausible answer and exits 0. A session
token (`raydock auth token`, `rls_…`) is the wrong credential: it clears the 401 and
returns 403 from the model data plane. See README ⁷.

---

## 5. NOT SUPPORTED (do not run; mark as not supported)

| Mode | Why |
|---|---|
| **Rcl-K** | `--local-model=on` is on the **main** (agent) class; the main never goes local (may-local is subagents-only) → the `Rcl` engine is **inert**; ≡ Rc-K. |
| **Rcl-L** | same as Rcl-K on the main class → inert; ≡ Rc-L. |
| **L-Rcl** | subagent may-local would need the LSR to defer to the RCR (advertise + follow the 307); not implemented. The naive on-device shortcut collapsed to L-L and was reverted. |

---

## 6. What to report

For each of the 16 supported modes, output one line:

`MODE — <PASS|REVIEW|FAIL> — main: <target/model>, subagents: <observed> — <note>`

- **PASS**: deterministic expectation met (Rl-Rl, Rl-L, S-Rl, Rc-L, Rc-L-per-type, S-L).
- **REVIEW**: hosted/non-deterministic (cloud pick, may-local) or local-main — state what was observed and whether it's consistent with the expected intent.
- **FAIL**: a deterministic expectation was wrong, OR an env prereq blocked it (say which, e.g. "no ANTHROPIC_API_KEY").

Then list Rcl-K/Rcl-L/L-Rcl as **NOT SUPPORTED**. Finish with a one-line summary
(e.g. "6 PASS, 8 REVIEW, 2 FAIL-due-to-missing-key, 3 not supported") and call out
any genuine routing bug (a deterministic row that didn't match).
