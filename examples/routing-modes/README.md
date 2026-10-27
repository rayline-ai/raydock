# Routing-mode configs for `--config`

Each file here is a `RouterConfig` (`endpoints` + `routes`) you can drive with
any of these entry points (each is a column in the [Modes](#modes) table):

- **interactive:** `raydock claude --config ./examples/routing-modes/Rc-L.json`
- **headless / agents:** `raydock router start --config ./examples/routing-modes/Rc-L.json`
  (then point an Anthropic SDK client at the proxy on `127.0.0.1:20810` — and see ⁷
  for the router key it needs)
- **Codex:** `raydock codex --config ./examples/routing-modes/Rc-Rc.json`, or
  `--auth subscription` for the `S-*` modes (⁵)

`--config` drives **both** the main agent (`routes.main`) and subagents
(`routes.subagent`) from one file — the thing the old `--router-config-path` /
`settings.json` surfaces could not express (they are subagent-only).

`routes.subagent` is **optional**: with only `routes.main`, subagents inherit the
main route, so a one-model config needs a single entry — this ships as
[`K.json`](./K.json). The split modes below spell both out precisely because main
and subagents differ — including [`K-K.json`](./K-K.json), the two-model form of
`K.json` (one keyed endpoint, main `kimi-k2.6`, subagents `glm-4.6`).

> **Scope.** The modes that ship a config file route end-to-end today — the
> per-entry-point **Claude / Codex / Router** columns below say where each works —
> including `Rcl-Rcl`/`S-Rcl` (may-local from config) and `Rl-Rl` (on-device LSR
> static routing). The modes with no `.json` yet (the fully-❌ rows) are listed for
> completeness. The full design lives in the routing-modes design doc.

## Mode names

The mode name is two tokens joined by a dash — **`main-subagent`** — naming the
routing class for the **main** agent (left of the dash) and its **subagents**
(right), e.g. `Rc-L` (cloud-router main, local subagents) or `S-Rc` (subscription
main, cloud-router subagents).

Each token is one of four provider classes:

- **S — Subscription** (first-party frontier OAuth): resolves to **Anthropic
  (Claude)** under `raydock claude` and to **OpenAI (ChatGPT)** under
  `raydock codex`. Uses your interactive login. **Main leg only** — a subagent can't
  be a subscription (see †).
- **K — Keyed** provider API: the same first-party providers (Anthropic / OpenAI)
  **or any OpenAI-compatible endpoint** (e.g. OpenRouter), authenticated by an
  **API key** (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, …) rather than a
  subscription.
- **R — Rayline**: rayline is a routing *system*, not a single destination, so this
  class always carries an **engine** suffix glued onto the `R` — `Rc`, `Rcl`, or
  `Rl` (below). `R` never appears bare.
- **L — Local** on-device model (your ollama).

**S and K are the same providers split by auth mechanism** — S is your subscription
OAuth, K is an API key. The split is not cosmetic: a **main** agent can pass through
your subscription (`S`), but a **subagent** is the *routed* class and the router
cannot forward subscription OAuth, so a routed first-party subagent is always **`K`**
(API key). There is therefore **no `-S` subagent form** — see †.

### The `R` engine suffix

**S**, **K**, and **L** are fixed **destinations** — the provider *is* the endpoint,
so there is nothing left to decide, and the `router` / `local-model` columns are
`N/A` for them. **`raydock` is different: it is a routing *system*.** Choosing it
opens two sub-axes that apply only to `raydock`, and they **nest** — `router`
(`rayline-cloud` | `rayline-local`), then *only under `rayline-cloud`*,
`local-model` (`on` | `off`). That gives a `raydock` class **three** behaviours, one
per engine token:

| token | `router` | `local-model` | a `raydock` class is then… |
|---|---|---|---|
| `Rc` | rayline-cloud | off | the hosted **RCR** (intelligent ML pick) serves a **cloud** model only |
| `Rcl` | rayline-cloud | on | the RCR may **redirect to a local model** (may-local) — today **`Explore` subagents only; main stays cloud** |
| `Rl` | rayline-local | — (N/A) | the on-device **LSR** routes it **statically per the JSON** and pins the route's `model`, even when the endpoint is `rayline-cloud` |

The `rayline-` prefix keeps the *engine* distinct from the *provider*:
`router: rayline-local` is not the same thing as `subagent: local` (the `L` class).

## Modes

The three support columns are the three entry points that drive a config:
**Claude** (`raydock claude --config`), **Codex** (`raydock codex --config`),
**Router** (`raydock router start --config`, then point an SDK client at the proxy).
They share one routing engine, so they agree except where an entry point adds a
constraint the engine can't lift — Codex's sentinel-model rule, the local main's
dependence on a pinned context window, and the Router column's credential
requirement.

- **✅** — works end-to-end.
- **🟡** — routes correctly, but the leg past the main agent is unverified
  end-to-end. Only the Codex local-main cells are 🟡; see ⁴.
- **❌** — not supported (², ³).

Every shipped config's routing is exercised by the [hermetic tests](#tests)
regardless of column status.

| Mode | agent | subagent | router | local-model | Main agent → | Subagents → | Auth | Claude | Codex | Router | Config |
|---|---|---|---|---|---|---|---|:--:|:--:|:--:|---|
| **Rc-Rc** | `raydock` | `raydock` | rayline-cloud | off | cloud (RCR) | cloud (RCR) | rayline | ✅ | ✅ | ✅ | [`Rc-Rc.json`](./Rc-Rc.json) |
| **Rcl-Rcl** § | `raydock` | `raydock` | rayline-cloud | on | cloud (RCR) § | cloud model (RCR may send a subagent → local) | rayline | ✅ | ✅ | ✅ | [`Rcl-Rcl.json`](./Rcl-Rcl.json) |
| **Rl-Rl** | `raydock` | `raydock` | rayline-local | N/A | cloud model (via local router) | cloud model (via local router) | rayline | ✅ | ✅ | ✅ | [`Rl-Rl.json`](./Rl-Rl.json) |
| **Rc-K** † | `raydock` | `keyed` | rayline-cloud | off | cloud (RCR) | Anthropic (API key) | rayline + Anthropic key | ✅ | ✅ | ✅ | [`Rc-K.json`](./Rc-K.json) |
| **Rcl-K** ² | `raydock` | `keyed` | rayline-cloud | on | cloud model (RCR may send an agent → local) | Anthropic (API key) | rayline + Anthropic key | ❌ | ❌ | ❌ | — (may-local) |
| **Rl-K** † | `raydock` | `keyed` | rayline-local | N/A | cloud model (via local router) | Anthropic (API key) | rayline + Anthropic key | ✅ | ✅ | ✅ | [`Rl-K.json`](./Rl-K.json) |
| **Rc-L** | `raydock` | `local` | rayline-cloud | off | cloud (RCR) | local model | rayline | ✅ | ✅ | ✅ | [`Rc-L.json`](./Rc-L.json) |
| **Rcl-L** ² | `raydock` | `local` | rayline-cloud | on | cloud model (RCR may send an agent → local) | local model | rayline | ❌ | ❌ | ❌ | — (may-local) |
| **Rl-L** | `raydock` | `local` | rayline-local | N/A | cloud model (via local router) | local model | rayline | ✅ | ✅ | ✅ | [`Rl-L.json`](./Rl-L.json) |
| **S-Rc** | `subscription` | `raydock` | rayline-cloud | off | subscription (Claude / ChatGPT) | cloud (RCR) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`S-Rc.json`](./S-Rc.json) |
| **S-Rcl** § | `subscription` | `raydock` | rayline-cloud | on | subscription (Claude / ChatGPT) | cloud model (RCR may send a subagent → local) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`S-Rcl.json`](./S-Rcl.json) |
| **S-Rl** | `subscription` | `raydock` | rayline-local | N/A | subscription (Claude / ChatGPT) | cloud model (via local router) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`S-Rl.json`](./S-Rl.json) |
| **S-L** | `subscription` | `local` | N/A | N/A | subscription (Claude / ChatGPT) | local model | subscription | ✅ | ✅ ⁵ | ✅ | [`S-L.json`](./S-L.json) |
| **K-K** ⁶ | `keyed` | `keyed` | N/A | N/A | keyed provider (API key) | keyed provider (API key, distinct model) | provider API key | ✅ | ✅ ⁶ | ✅ | [`K-K.json`](./K-K.json) |
| **L-Rc** ¹ | `local` | `raydock` | rayline-cloud | off | local model | cloud (RCR) | rayline | ✅ | 🟡 ⁴ | ✅ ⁷ | [`L-Rc.json`](./L-Rc.json) |
| **L-Rcl** ³ | `local` | `raydock` | rayline-cloud | on | local model | cloud model (RCR may send a subagent → local) | rayline | ❌ | ❌ | ❌ | — (may-local) |
| **L-Rl** ¹ | `local` | `raydock` | rayline-local | N/A | local model | cloud model (via local router) | rayline | ✅ | 🟡 ⁴ | ✅ ⁷ | [`L-Rl.json`](./L-Rl.json) |
| **L-K** ¹ | `local` | `keyed` | N/A | N/A | local model | Anthropic (API key) | API key | ✅ | 🟡 ⁴ | ✅ | [`L-K.json`](./L-K.json) |
| **L-L** ¹ | `local` | `local` | N/A | N/A | local model | local model | none | ✅ | 🟡 ⁴ | ✅ ⁷ | [`L-L.json`](./L-L.json) |

Plus three granular **per-type** variants that split subagents by **type**
instead of one blanket default — all three are ✅ on Claude, Codex and Router:

- [`Rc-L-per-type.json`](./Rc-L-per-type.json) — main cloud; `Explore`/`Plan` →
  distinct local models, everything else → cloud.
- [`S-L-per-type.json`](./S-L-per-type.json) — main on your Claude **subscription**;
  only `Explore` → local, and **every other subagent passes through to the
  subscription** (no `routes.subagent` default). The selective counterpart of `S-L`
  (which sends *all* subagents local): because subagents can't be *routed* to the
  subscription (the `†` rule), the non-local ones are left un-routed so they pass
  through with the main. Verified on-device: main + `general-purpose` →
  `target=anthropic`, `Explore` → `target=remote model=qwen2.5-coder:7b`.
- [`S-Rc-per-type.json`](./S-Rc-per-type.json) — same trick pointed at
  `rayline-cloud` instead of local: main on the subscription, only `Explore` → the
  **RCR**, every other subagent passes through. Selective counterpart of `S-Rc`.

**† there is no `-S` (subscription subagent) form.** Subagents are the *routed*
class and the router cannot forward your Claude/ChatGPT subscription OAuth, so a
routed first-party subagent authenticates by **API key** — the **`-K`** form
(`Rc-K`/`Rl-K`, and `L-K`, all `ANTHROPIC_API_KEY`). Swap a subscription in on the
subagent leg and only that leg breaks; use `-K` there instead. (`S` still works on
the **main** leg, which passes through rather than being routed.) The intent columns
show what the mode *means*.

---

**¹ `agent = local` (`L-Rc`/`L-Rl`/`L-K`/`L-L`) — pin the local model's context
window first.** These run the **main** agent on a local model. With the window
pinned, the local main drives the full tool loop and spawns subagents; with ollama's
VRAM-derived default it does not, and the failure looks like a capability limit but
is not.

**Pin the window.** ollama sizes a model's context at load time
(`OLLAMA_CONTEXT_LENGTH`, *"default: 4k/32k/256k based on VRAM"*), so a
VRAM-constrained host silently gets **4096 tokens** — confirm with `ollama ps`,
CONTEXT column. An agent harness's system prompt plus its tool schemas does not fit
in that; the overflow truncates the **tool definitions**, and a model handed no tools
does the only thing left — it narrates the call as prose. The tell that this is the
context window and not the model: a 36B and a 9B fail *identically*, at the same
prompt-size threshold, and both emit proper `tool_use` at `num_ctx` 32768.

Rayline cannot inject this: ollama drops `options` on both of its compat routes
(`/v1/chat/completions`, `/v1/messages`) — the native `/api/chat` honors
`options.num_ctx`, so the drop is specific to the compat shims Rayline's
`openai_chat` endpoints speak. Bake it into the tag instead, which survives them —

```
FROM qwen3.5:9b
PARAMETER num_ctx 32768
```
```
ollama create qwen3.5:9b-32k -f Modelfile   # then name that tag in `models` and `routes.*.model`
```

— or raise `OLLAMA_CONTEXT_LENGTH` on the ollama server.

**With the window pinned, all four modes complete end-to-end on Claude and Router**
(measured with `qwen3.5:9b` @ 32768 on an Apple M3 / 16 GB, prompt spawning an
`Explore` **and** a `general-purpose` subagent in parallel): the local main drives
the tool loop, spawns both subagents, receives their results, and answers. It also
recovers from its own tool-schema mistakes — given a wrong parameter set it re-read
the schema and retried correctly. Per-mode subagent destinations are exactly what the
table says: `endpoint:ollama` for `L-L`, `endpoint:rayline-cloud` for `L-Rc`, the
pinned `deepseek/deepseek-v4-pro` for `L-Rl`, and `endpoint:anthropic` →
`claude-sonnet-5` for `L-K`.

Three things to know when reading logs for these modes:

- **Spawning is flaky.** Roughly one attempt in three or four, the local main
  narrates *"I'll spawn a general-purpose subagent…"* and stops instead of calling
  the tool. A cloud main does not do this. Treat ✅ as "works", not "works every
  time", and re-run before concluding a mode is broken. Runs are also slow — minutes
  to reach the spawn.
- **On `L-K`, each subagent's first request returns HTTP 400** and succeeds on the
  client's retry. A 400 pair on that leg is not a failure.
- **On a route-all local-main config, Claude Code's background `claude-sonnet-5`
  traffic** falls through to the built-in `anthropic` endpoint and logs
  `requires $ANTHROPIC_API_KEY`. Noisy, harmless, not the mode failing.

The `Router` column needs the client pointed at the **proxy** (`:20810`), not the
injector (`:20809`): attaching to the injector loses the subagent header and every
request classifies as `task=main`. It also needs an `rlk-` key — see ⁷.

The live e2e test (`it_local_main_e2e`, `#[ignore]`d) is the harness for pinning this
down across more hosts and models.

**² ❌ may-local is inert (`Rcl-K`/`Rcl-L`).** may-local (`Rcl`) only ever redirects
**cloud-routed `Explore` subagents** to local — never the main agent. In
`Rcl-K`/`Rcl-L` the only `rayline-cloud` class is the **main** (the subagent is
Anthropic in `Rcl-K`, already local in `Rcl-L`), so the `Rcl` engine lands on the
main, where it has no effect — and there is no cloud-routed subagent to carry it
instead. The flag does nothing: `Rcl-K` ≡ `Rc-K` and `Rcl-L` ≡ `Rc-L`. They aren't
shipped as distinct modes, to avoid implying a behavior that doesn't exist. (Contrast
`Rcl-Rcl`/`S-Rcl`, whose subagent **is** cloud-routed, so may-local applies and they
ship — see §.)

**³ ❌ may-local handoff not built (`L-Rcl`).** Different reason from ²: here the
**main** is local, which engages the **on-device** router for the whole run. Making
the cloud subagent honor may-local would require the on-device LSR to advertise the
local model and **defer the decision back to the hosted RCR** (a 307 handoff) — that
path isn't implemented, so `L-Rcl` is not shipped as a distinct mode.

**§ may-local from config — custom-endpoint scope + hosted decision.** `Rcl-Rcl`
wires the **client** contract from config: a `rayline-cloud` route carrying
`local_models` stands up a custom adapter fronting the named local endpoint and
advertises it to the RCR (`x-rayline-local-available` + the model id), decoupled from
the `raydock local on/off` account toggle. Two important limits:

- **Exploration subagents only (custom endpoint).** A config `local_models` points at
  a *custom* local endpoint (your ollama), so the proxy sends
  `x-rayline-local-custom` and the RCR **only delegates exploration subagents
  (`Explore`) to it — never the main agent** or other classes (`proxy/src/lib.rs`
  `custom_mode`). The "Main agent →" column above is therefore the *intent*; in
  practice the main turn stays cloud. Main-agent may-local would need a
  trusted/bundled model path or a hosted change.
- **The redirect is the hosted RCR's call.** Whether a qualifying turn is actually
  sent to local is the RCR's runtime decision; without it (or with the account flag
  off) `Rcl-Rcl` behaves like `Rc-Rc`.

Verified on-device: `raydock claude --config Rcl-Rcl.json` with an `Explore`-spawning
task → adapter forwards to `http://<endpoint>/v1/messages` and `raydock top` shows
`model=<local>, target=local, agent_type=Explore` while main turns stay cloud.
**`S-Rcl`** works the same way (main passes through to the subscription; the cloud
subagent advertises may-local) and is likewise verified on-device. The advertisement
+ redirect *plumbing* is hermetically tested; the end-to-end redirect is exercised
only by the ignored live test.

**⁴ 🟡 Codex, local main (`L-Rc`/`L-Rl`/`L-K`/`L-L`) — no subagent tool to route.**
Codex sends a **sentinel `--model`** (`rayline-local` by default) on every turn; the
`--config` codex materialization pins that sentinel to `routes.main` for **main**
turns, while the router skips that model_route on **subagent** turns so subagent
routing applies.

The **main leg works**: the sentinel routes to the config's on-device endpoint, and
with the window pinned (¹) the local model drives Codex's agentic tool loop —
multi-turn, real `exec_command` calls, coherent final answer. `raydock codex --config
L-L.json` → `codex route endpoint:ollama requested=rayline-local
selected=qwen3.5:9b-32k`.

The **subagent leg cannot be exercised**, which is what keeps these 🟡. On codex-cli
0.145.0 `exec`, no subagent tool is exposed to the model at all — `collab_spawn` sits
behind the under-development `multi_agent` / `collaboration_modes` features, and
enabling both (`-c features.multi_agent=true -c features.collaboration_modes=true`)
does not surface it. Driving `L-K` makes this explicit: the main routes correctly and
the model then reports it has no subagent-spawning tool available. With nothing to
spawn there is nothing to route through `routes.subagent`/`routes.subagents`.

This is a client-capability gap, not a Rayline one, and it applies to all four modes
identically (the main leg is the same sentinel → same local route in each). Treat
`collab_spawn` as **version-dependent** throughout this document: confirm your codex
build actually offers the tool before relying on a main≠subagent split.

The **cloud-main (`Rc*`/`Rcl*`/`Rl*`) modes are plain ✅** — the codex `--config`
materialization flips the hosted RCR endpoint to `openai_responses` (bearer auth,
`x-rayline-client: codex`), so a Codex `/v1/responses` main forwards natively (`POST
/v1/responses`) and the RCR picks the model — no down-translation to Anthropic. The
RCR reports the chosen model back via `x-rayline-selected-model`, so `raydock top`
shows the real model. Only a user's own **custom** `anthropic_messages` endpoint is
left untouched (host-guarded to `api.rayline.ai` / `api-dev.rayline.ai`). Verified
end-to-end against prod and dev: `raydock codex --config Rc-Rc.json` drives a real
Codex session through the RCR (trivial prompt → cheap tier, hard prompt → frontier).

**⁵ ✅ Codex — subscription main via `--auth subscription`.** The `S-*` modes route
`routes.main` to the `subscription` sentinel, which `raydock codex --auth
subscription` materializes into a Codex `client_bearer` endpoint
(`chatgpt.com/backend-api/codex`) and pins the sentinel model to it for main turns.
Subagent turns skip that pin and follow `routes.subagent`/`routes.subagents`, so
`S-Rc`/`S-Rcl`/`S-Rl`/`S-L` diverge on the subagent leg (main → your ChatGPT
subscription, subagents per config). Verified on-device: `raydock codex --auth
subscription --config S-L.json` (and `S-Rc.json`) → `codex route
endpoint:codex-subscription requested=rayline-local selected=gpt-6-astra` → reply
returned. Run it exactly as written — the default model routes correctly.

**⁶ K-K — one keyed endpoint, two models (main ≠ subagent).** `K-K` declares one
**keyed** provider endpoint (the shipped config uses OpenRouter,
`OPENROUTER_API_KEY`) whose `models` lists two entries, then splits them across legs:
`routes.main` → `moonshotai/kimi-k2.6`, `routes.subagent` → `z-ai/glm-4.6`. Both legs
are the same keyed provider and auth; only the model differs — the pure-keyed
analogue of a frontier-main / cheaper-subagent split. Drop `routes.subagent` (and the
second model) and subagents fall back to **inheriting the main route** — the minimal
one-model form. Swap the endpoint/`models` for any OpenAI-compatible or
Anthropic-API-key provider. On **Codex**, a keyed `anthropic_messages` main is served
via the standard down-translation path (Responses → Anthropic Messages → provider),
not the RCR-native Responses path reserved for the hosted router (⁴).

**⁷ Router column — `raydock router start` needs an `rlk-` router key.** `rayline
claude` attaches your credential automatically; `raydock router start` injects
nothing. So any mode whose **subagent** leg is a cloud endpoint (`L-Rc`, `L-Rl`, and
the `*-Rc*`/`*-Rl` family generally) routes correctly and then fails upstream on that
leg with **401** (no key) or **403** (wrong kind of key).

The two credentials are not interchangeable:

| Credential | Looks like | Plane | Data-plane result |
|---|---|---|---|
| session token (`raydock auth token`) | `rls_…` | control plane | **403** `authentication_error` — *"Rayline session tokens cannot call the model data plane. Use an rlk- router key."* |
| router key (`rayline key create`) | `rlk-…` | model data plane | ✅ |

```bash
export RAYLINE_ROUTER_API_KEY="rlk-…"    # NOT $(raydock auth token)
raydock router start --config examples/routing-modes/L-Rc.json
```

The 403 is identical for an unpinned `rayline-router` route, so it is never about
which model a route pins.

**Check the router log, not just the exit status.** On a local-main config the main
answers regardless, so the run prints a plausible final message and exits 0 while the
subagent leg silently retries and gives up. A printed answer is not evidence the
subagent leg worked — grep for 401/403 on `task=subagent` before scoring a cell.

## Files ↔ modes

The supported modes ship as **19 config files** (the `❌` modes have none yet),
plus one non-mode minimal config ([`K.json`](./K.json), below the table):

| File | `routes.main` → | `routes.subagent` → | Mode |
|---|---|---|---|
| [`Rc-Rc.json`](./Rc-Rc.json) | rayline-cloud | rayline-cloud | Rc-Rc |
| [`Rcl-Rcl.json`](./Rcl-Rcl.json) | rayline-cloud (+ `local_models`) | rayline-cloud (+ `local_models`) | Rcl-Rcl § |
| [`Rl-Rl.json`](./Rl-Rl.json) | rayline-cloud, `router: rayline-local` (model pinned) | rayline-cloud, `router: rayline-local` (default + `Explore` per-type) | Rl-Rl |
| [`Rc-K.json`](./Rc-K.json) | rayline-cloud | anthropic (API key) | Rc-K |
| [`Rl-K.json`](./Rl-K.json) | rayline-cloud, `router: rayline-local` (model pinned) | anthropic (API key) | Rl-K |
| [`Rc-L.json`](./Rc-L.json) | rayline-cloud | ollama (local) | Rc-L |
| [`Rl-L.json`](./Rl-L.json) | rayline-cloud, `router: rayline-local` (model pinned) | ollama (local) | Rl-L |
| [`S-Rc.json`](./S-Rc.json) | subscription (passthrough) | rayline-cloud | S-Rc |
| [`S-Rcl.json`](./S-Rcl.json) | subscription (passthrough) | rayline-cloud (+ `local_models`) | S-Rcl § |
| [`S-Rl.json`](./S-Rl.json) | subscription (passthrough) | rayline-cloud, `router: rayline-local` (model pinned) | S-Rl |
| [`S-L.json`](./S-L.json) | subscription (passthrough) | ollama (local) | S-L |
| [`K-K.json`](./K-K.json) | openrouter (keyed, kimi-k2.6) | openrouter (keyed, glm-4.6) | K-K ⁶ |
| [`L-Rc.json`](./L-Rc.json) | ollama (local) | rayline-cloud | L-Rc |
| [`L-Rl.json`](./L-Rl.json) | ollama (local) | rayline-cloud, `router: rayline-local` (model pinned) | L-Rl |
| [`L-K.json`](./L-K.json) | ollama (local) | anthropic (API key) | L-K |
| [`L-L.json`](./L-L.json) | ollama (local) | ollama (local) | L-L |
| [`Rc-L-per-type.json`](./Rc-L-per-type.json) | rayline-cloud | per-type: `Explore`/`Plan` → ollama, default → rayline-cloud | Rc-L\* |
| [`S-L-per-type.json`](./S-L-per-type.json) | subscription (passthrough) | per-type: `Explore` → ollama, all other subagents → subscription (passthrough) | S-L\* |
| [`S-Rc-per-type.json`](./S-Rc-per-type.json) | subscription (passthrough) | per-type: `Explore` → rayline-cloud (RCR), all other subagents → subscription (passthrough) | S-Rc\* |

Plus a minimal single-model config that is **not** a distinct mode:

| File | `routes.main` → | `routes.subagent` → | Mode |
|---|---|---|---|
| [`K.json`](./K.json) | openrouter (keyed, kimi-k2.6) | *(none — subagents inherit main)* | K-K (one-model form) |

`K.json` is the degenerate one-model form of `K-K`: a single keyed endpoint with only
`routes.main`, so subagents inherit it. Add a `routes.subagent` on a second model and
you have `K-K`.

The `L-*.json` configs name the plain `qwen3.5:9b` tag. To run them you will want a
context-pinned tag instead — see ¹.

The proxy **scope** is derived from `routes.main`:

- `routes.main.endpoint == "subscription"` (a reserved sentinel) **or absent** → the
  main agent passes through to your own Claude subscription (selective-subagents
  scope). You do **not** declare `subscription` under `endpoints`.
- `routes.main` → any real endpoint → the main agent is routed (route-all scope).

## Config model — `endpoints` + `routes`

Real `EndpointConfig` fields only: `id`, `protocol`
(`anthropic_messages` | `openai_chat` | `openai_responses`), `base_url`, `models`,
`api_key_env`, `auth` (`api_key` | `bearer`), `headers`. A route's `endpoint` is
looked up by `id`; its `model` is rewritten into the request body.

```jsonc
{
  "endpoints": [
    { "id": "rayline-cloud", "protocol": "anthropic_messages", "base_url": "https://api.rayline.ai",
      "models": ["rayline-router"], "api_key_env": "RAYLINE_ROUTER_API_KEY", "auth": "api_key" },
    { "id": "ollama", "protocol": "openai_chat", "base_url": "http://127.0.0.1:11434/v1",
      "models": ["qwen3.5:9b", "qwen2.5-coder:7b"] }     // local, no auth
  ],
  "routes": {
    "main":     { "endpoint": "rayline-cloud", "model": "rayline-router" },
    "subagent": { "endpoint": "ollama", "model": "qwen3.5:9b" },   // singular = subagent default
    "subagents": {                                                 // optional per-type overrides
      "Explore": { "endpoint": "ollama", "model": "qwen2.5-coder:7b" }
    }
  }
}
```

> Note: `routes.subagent` (singular) is the subagent **default**;
> `routes.subagents` (the map) is **only** for per-type overrides.

### What `model` means at the `rayline-cloud` endpoint

A route's `model` is rewritten into the request body, so at the hosted RCR it is the
whole instruction. Two values, two meanings:

| `model` on the wire | RCR behaviour |
|---|---|
| `rayline-router` (or absent) | **route** — the RCR's rules/ML pick the model |
| any concrete id (`claude-opus-5`, `z-ai/glm-5.2`, `gpt-5.5`, …) | **pin** — that exact model is used, and the rules/ML tiering, stickiness and ML override are all skipped |

The rule is the same for every provider. Asking to be routed is what `rayline-router`
is *for*; naming a model instead means you want that model.

Claude ids may also be written provider-qualified: `anthropic/claude-opus-5` pins
exactly like the bare spelling, on `/v1/models` as well as `/v1/messages`. The prefix
has to agree with the id it qualifies — `anthropic/gpt-5.5` is rejected with a 404
rather than dispatched to OpenAI under an Anthropic label.

#### Where a pin does not apply

Two lanes ignore a `claude-*` id, because there it is whatever the Claude Code
session happened to be on rather than a deliberate choice:

- **haiku ids** are intercepted — Claude Code emits them for background work
  (auto-title, compact, tool-result analysis), so they are redirected regardless.
- **subagent turns** run the delegated-subagent policy, so a `claude-*` value in
  `routes.subagent.model` does not pin at the RCR.

Two details of that second lane matter when you are picking a subagent model:

- It covers **inherited Claude ids only**. A non-Anthropic `routes.subagent.model`
  (`z-ai/glm-5.2`, `gpt-5.5`) is configuration rather than inheritance, so it pins on
  subagent turns like anywhere else.
- It is **not** keyed on one header. `x-claude-code-agent-id` and
  `x-rayline-subagent` both assert a subagent turn, and failing those the RCR falls
  back to the prompt shape — so header-less clients (review-agent, the Agent SDK)
  land in this lane too.

Use `router: rayline-local` (`Rl`) to decide subagent models on-device and avoid the
question entirely.

The `rayline-router` sentinel is treated the same way on a subagent turn: it runs the
delegated policy, so `rules.delegated_subagent` applies instead of the balanced
policy's cheap slot. Naming a specific policy — `workshop-router-fast`,
`workshop-router-frontier` — is a deliberate choice and is honoured as written.

### `raydock`-only route fields

A route targeting the `raydock` cloud endpoint accepts two optional fields:

```jsonc
"main": {
  "endpoint": "rayline-cloud", "model": "rayline-router",
  "router": "rayline-cloud",              // rayline-cloud (default) | rayline-local (Rl)
  "local_models": ["qwen2.5-coder:7b"]    // non-empty ⇒ may-local ON (Rcl); must be served by a declared local endpoint
}
```

- **`router`** — which rayline decider runs. `rayline-cloud` (or absent) = the hosted
  RCR picks the model. `rayline-local` (`Rl`) = the **on-device LSR** is the router:
  it engages even when the endpoint is `rayline-cloud`, routing the class statically
  per the JSON and **pinning the route's `model`** (so the RCR doesn't pick). The
  `model` you set is sent as-is to the endpoint.
- **`local_models`** — the model ids the cloud RCR may redirect this class to
  (may-local, `Rcl`). A non-empty list turns may-local **on** and advertises
  `local_models[0]`; the id must appear in a declared local endpoint's `models` (that
  endpoint's `base_url` is the redirect target). Both fields are ignored for
  `S`/`K`/`L` endpoints and do not change the local router's own routing.

## Auth

- `rayline-cloud` reads `RAYLINE_ROUTER_API_KEY`, which must be an **`rlk-` router
  key**. For `raydock claude`, your `raydock auth login` credential is injected
  automatically, so the env var is optional in interactive use. `raydock router
  start` injects nothing and always needs the key — see ⁷.
- `keyed` (`K`) reads the provider's API key from the endpoint's `api_key_env`
  (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, …) — the local router cannot use a
  subscription (see †).
- `ollama` needs no key (point `base_url` at your server).

## Visual demo

[`visual-test.sh`](./visual-test.sh) records an asciinema cast of any mode in a
split-pane tmux session — the left pane runs the client and the right pane runs
`raydock top`, so you can watch the routing live:

```bash
./examples/routing-modes/visual-test.sh Rc-Rc                       # default mode + default prompt
./examples/routing-modes/visual-test.sh S-Rcl                       # any supported mode from the table
./examples/routing-modes/visual-test.sh S-Rcl "your prompt here"    # 2nd arg overrides the prompt
```

The **default Claude prompt spawns subagents**, so subagent routing — and may-local
(e.g. `Rcl-Rcl`/`S-Rcl`, where only `Explore` subagents go local) — is actually
visible in `raydock top`. `CLIENT=codex` drives Codex instead; `DEMO_HEADLESS=1`
skips the recording and prints a text transcript.

Requires `asciinema` and `tmux` for the recorded path (and a TTY). It forces
`--via proxy` for Claude (so `raydock top` has metrics to show) and writes
`<MODE>-demo.cast`; play it with `asciinema play <MODE>-demo.cast`.
[`VISUAL-TEST.md`](./VISUAL-TEST.md) is the full per-mode verification runbook.

## Tests

Routing is regression-tested hermetically (no credentials, loopback-only):

- **Every supported config in this directory** is swept in `rayline-local-router`
  unit tests — `config_mode_examples_route_main_and_subagents` loads each `*.json`
  and asserts the main + subagent (+ per-type `Explore`/`Plan`) routing decision, and
  `example_configs_parse` asserts they all deserialize.
- **Per-mode CLI derivation** in `rayline-cli` —
  `example_mode_configs_derive_expected_routing` cross-checks each config's derived
  proxy scope, local-router engagement, and cloud-key need against the mode's intent.
- **Full HTTP path** in `crates/rayline-local-router/tests/it_mock_upstream.rs`
  (`config_routes_main_and_subagent_to_distinct_endpoints`): mock upstreams stand in
  for each endpoint; a main request (no agent headers) and a subagent request
  (`x-claude-code-agent-id` + `x-rayline-claude-code-agent-type`) prove each class
  routes to its configured endpoint over real HTTP.

The selective-main-subscription passthrough (`S-Rc`/`S-L` main) is a proxy-layer
behavior, covered in `crates/rayline-proxy`.

**may-local — `Rcl-Rcl` (§).** The config→advertisement mapping is unit-tested in
`rayline-cli` (`router_config::tests::may_local_*` and
`rrcl_example_resolves_may_local`): a `rayline-cloud` route with `local_models`
resolves to the advertised model + the local endpoint's upstream URL, and
`rayline-local`/no-`local_models` routes resolve to none.
`config_mode_examples_route_main_and_subagents` also asserts the
`router`/`local_models` fields **do not** change the LSR's routing. The proxy half —
advertising `x-rayline-local-available` and following the router's `307` to the local
adapter — is hermetically tested in `crates/rayline-proxy`
(`proxy_stashes_router_auth_for_local_307`,
`local_proxy_redirect_uses_shared_router_auth_for_usage_update`). The **actual
redirect decision** is the hosted RCR's call and is account-gated, so the true
end-to-end is not — and cannot be — a hermetic config test; that path is exercised by
the ignored live test `crates/rayline-proxy/tests/it_claude_live.rs`.

The full **interactive** end-to-end for the `agent = local` modes (¹) is kept
`#[ignore]`d in `crates/rayline-cli/tests/it_local_main_e2e.rs` — it needs a local
model, a real `claude` binary, and a **pinned context window**, and even then the
spawn is flaky enough that a single run is not a reliable signal:

```bash
CLAUDE_BIN=/path/to/claude RAYLINE_LOCAL_MAIN_E2E=1 \
  cargo test -p rayline-cli --test it_local_main_e2e -- --ignored --nocapture
```
