# Hybrid routing with a local model: setup

Hybrid routing keeps easy work on a model running on your machine and sends
harder work, or work that no longer fits the local context window, to a cloud
model. ARC, the hosted policy, makes each decision. The `rayline-core`
executor runs it, and your coding harness runs unmodified.

This page covers the user setup. For the process contract, configuration
reference and validation scripts, see [ARC hybrid execution](hybrid-routing.md).

## Requirements

- macOS 13 or later, or Linux, with a Rayline release installed. The release
  includes `raydock`, `rld` and `rayline-core`.
- A local inference server with your Qwen model: llama.cpp, Ollama or MLX.
  Rayline does not start, download or stop it.
- Memory for Qwen3.8-27B: plan for at least 32 GB (unified memory on a Mac).
  At 4-bit the weights alone are about 17 GB (Ollama `qwen3.8:27b`), before
  the cache for the 98,304-token default window, so a 16 GB machine cannot
  run it.
- Nothing else. A profile file is optional (see [Advanced: a profile file](#advanced-a-profile-file)).

## Watch requests

After starting hybrid, run `raydock top --hybrid` to watch local/cloud execution,
or `raydock top --hybrid --json` for a single snapshot. For a separate instance,
add `--state-dir /path/to/instance`. No additional metrics port is needed.
Plain `raydock top` discovers the default live hybrid instance only when legacy
metrics are not also live; use `--hybrid` or `--legacy` to resolve that ambiguity.
These flags are mutually exclusive; `--state-dir` requires `--hybrid`. Unknown
execution models remain unknown rather than showing the harness's virtual model.

## What it costs

Every turn of a hybrid session bills one ARC decision against your Rayline
balance, including turns that run on your machine; a cloud turn additionally
bills the provider's pass-through price. The local model's price in the profile
(0 by default) steers which target ARC selects; it never changes what the
decision costs. See [ARC pricing](https://docs.rayline.ai/arc/pricing).

## Three steps

1. **Sign in.**

   ```bash
   raydock auth login
   ```

   `start` mints your Rayline API key on first use if none is saved. That key
   is used only for HTTPS endpoints on that environment's router host. An API
   key variable set in the launch environment (`RAYLINE_ROUTER_API_KEY`)
   always wins. Startup prints only the variable's name.

2. **Point Rayline at your local model.**

   ```bash
   raydock local custom --url http://127.0.0.1:8080/v1 --model qwen3.8-27b
   ```

   This is the local endpoint hybrid routing uses. It must serve Qwen3.8-27B
   (Hugging Face `Qwen/Qwen3.8-27B`; Ollama `qwen3.8:27b`), ARC's local model.
   `raydock hybrid start` reads the loaded model's name from llama.cpp and
   Ollama, and runs the session cloud only if it names another model. If the
   runtime reports no name, `start` warns that it cannot read the model and
   assumes Qwen3.8-27B. That includes Ollama models whose file carries no
   name metadata, so check the model yourself when you see that warning.

   To serve another model, declare it by its registry id in
   `~/.config/rayline/settings.json`:

   ```json
   {"hybrid": {"local_registry_model": "z-ai/glm-5.3-flash"}}
   ```

   The id is `provider/model`, as the execution core's model registry names
   it. Raydock checks only that shape. The core refuses a model its registry
   does not know when it starts; `start` then names this setting in its
   error. With a declared model, `start` skips the Qwen3.8-27B check and
   prints the declared model instead. Raydock does not check what the runtime
   serves, so declare the model it actually loads. The setting applies to the
   default profile only; `start --config` refuses it.

3. **Start and connect a harness.**

   ```bash
   raydock hybrid start
   raydock hybrid run pi -- "Fix the failing test in parser.py"
   raydock hybrid status
   raydock hybrid stop
   ```

   With no profile file, `start` builds the default profile:
   - Decisions come from your signed-in router's `arc-swift-1.0` model.
   - Your `local_model` is the local target.
   - Claude Opus 5.5, Sonnet 5.5 and Haiku 5.5, through the same router, are
     the cloud targets.

   Rayline uses the context window your runtime reports, so load it with the
   window you want:
   - llama.cpp: `llama-server --ctx-size 262144 ...` (Rayline reads the loaded
     window from `/props`);
   - vLLM and TensorFold: `--max-model-len 262144` (Rayline reads the served
     model's `max_model_len` or `context_length` from `/v1/models`);
   - Ollama and MLX report no window, so Rayline assumes 98,304 tokens. For
     Ollama set `OLLAMA_CONTEXT_LENGTH=98304 ollama serve`, or `num_ctx` in
     the model's Modelfile: Ollama's default is much smaller and it silently
     truncates a longer prompt. For MLX (`mlx_lm.server`), check that your
     build's KV-cache limit, if you set one, is at least that.

   For a model other than Qwen3.8-27B, Claude Code's opening request is
   about 73,000 tokens with no MCP servers or project instructions (more
   with them), and the window must also hold the answer (up to 32,000
   tokens). Below about 97,000 tokens Claude Code turns go to cloud, and
   `start` warns. 131,072 holds a plain session's first turns; give such
   a model 262,144 tokens to keep longer sessions local.

   Set the window yourself with `"hybrid": {"local_context_window": 262144}` in
   `~/.config/rayline/settings.json`, or `--local-context-window <tokens>` on
   `start`. A turn whose prompt does not fit the local window goes to a cloud
   model; harnesses are told the window is 1,000,000 tokens. `start` uses the
   production router by default; `--env dev` selects the development one.

   `start` checks Decisions once before it reports ready: a rejected key
   fails with the `raydock auth login` command to run, and a development cell
   (`--cell`) that is still starting is waited for.

   `start` prints the profile, the routing policy and the local model it bound.
   `run` also supports `claude`, `codex`, `opencode`, `omp`, `hermes` and
   `openclaw`. Start a fresh harness session after every `start`. With
   OpenCode 2, `run` adds `--standalone` so the session uses Rayline's
   configuration rather than OpenCode's shared background service.

   `raydock local test` checks the endpoint with an OpenAI Chat Completions
   request, the API hybrid routing uses.

## Pricing defaults

These are the prices Raydock tells ARC, which steer selection; they are not
what you are charged (see "What it costs" above). On-device inference costs
you about nothing per token. The default profile
therefore prices the local model at 0 per million tokens on every leg, sends
those caller prices (the cloud models keep their catalog prices), and asks ARC
Swift, which decides at its own fixed V and picks cheap. Under these defaults
easy work stays on device and harder work goes to the cloud. Change the local
price in the override block with `local_price` (USD per million tokens, applied
to every leg of the local model).

## Routing policy

With no override, Raydock sends the **hybrid default** preset,
`arc-policy.hybrid-default.v1`, on every decision. Under this preset:

- **The model** may change at cache boundaries: task start, a subagent start, a
  compaction, or a typed user turn whose cache is cold.
- **The thinking level** may change on every turn.
- **The capacity handoff** is always allowed (below).

Easy work stays on device. Harder work and context spills go to the cloud.

> **Keeping turns on your local model.** The hybrid default weighs each
> model's predicted chance of success against its price. Claude Haiku 5.5 is
> cheap, so when ARC predicts it is clearly more likely to succeed than your
> local model, the default picks Haiku even though local costs nothing. How
> often that happens depends on the task and the local model. To use the
> local model whenever ARC predicts it will pass, set:
>
> ```json
> {"hybrid": {"routing": {"policy": "prefer_local"}}}
> ```
>
> The threshold defaults to 0.65. Lower `min_p_pass` keeps more turns local.
> Below the threshold, `prefer_local` falls back to the default choice, so
> those turns can still go to Haiku.

### The optional override block

Every key in this block, in `~/.config/rayline/settings.json`, is optional:

```json
{"hybrid": {"routing": {"policy": "prefer_local",
                        "min_p_pass": 0.65,
                        "schedule": {"model": ["cache_boundaries", "user_messages"]}}}}
```

| Key | Meaning |
| --- | --- |
| `policy` | `utility` selects the hybrid default preset. `prefer_local` selects `arc-policy.prefer-local.v1`: the local model is used whenever ARC predicts it will pass (`p_pass >= min_p_pass`), and otherwise the default utility selection decides over every eligible target, so the local model can still win below the threshold. It works with the default profile and any local model. `start` refuses `prefer_local` when the session has no local target. |
| `min_p_pass` | The prefer-local threshold, in (0, 1). The service default is 0.65. It is valid only with `prefer_local`. |
| `schedule` | When the model (`model`) and thinking level (`thinking`) may change. Each is a set of classes: `cache_boundaries`, `user_messages` (any typed user turn) or `all_turns`. The service refuses a class its package does not support (`policy_schedule_not_admitted`). |
| `local_price` | Default profile only: the local model's price in USD per million tokens, applied to every leg (default 0). It must be 0 or between 1e-6 and 1e6. `start --config` refuses it, because a profile file sets its own prices. |

Unknown keys are refused. If `settings.json` cannot be read or is not valid
JSON, or the block is invalid, `start` stops with an error that names the file.
It never falls back to the default routing. A missing `settings.json` is fine.
A profile pinned to the older `current` or
`execution_context_v1` Decisions mode is passed through unchanged, and an
override block is refused for it.

## The capacity handoff

The local window is the profile's `context_window`. Before each request, the
core counts the exact prompt the local model would receive with the runtime's
own tokenizer. The count covers tools, history and private instructions. The
core adds the output limit, then tells ARC whether the request fits.

The default profile sets the local output limit to a quarter of the local
window, at most 32,000 tokens (24,576 at the default 96k window). Each cloud
target takes its own model's ceiling, 128,000 tokens on each of the three
Claude 5.5 pins, so a cloud answer is not capped at a fraction of what the
model allows. Set `hybrid.cloud_output_tokens` to declare those ceilings
yourself, keyed by the pinned model. The block names every pinned model or
`start` refuses it, so one written before Haiku was pinned needs a Haiku line:

```json
{
  "hybrid": {
    "cloud_output_tokens": {
      "claude-opus-5-5": 128000,
      "claude-sonnet-5-5": 128000,
      "claude-haiku-5-5": 128000
    }
  }
}
```

The core advertises the smallest of all the limits as the session's, so a long
local answer has room to finish, and a prompt larger than the local window
minus its answer allowance (a quarter of the window, at most 32,000 tokens)
goes to cloud. Each limit is enforced against the
deployment the turn lands on, not against that smallest figure: a harness that
asks for more than a deployment's ceiling has its turns on that deployment
lowered to it rather than refused.

When the conversation grows past the local window, for example during a long
tool loop with no compaction, ARC hands the turn to a cloud target. The receipt
reads `decided_by: capacity_handoff`, and the local target is listed as
excluded for `context_capacity`. The conversation continues on the cloud model.
No tool is replayed, and nothing is reselected mid-stream. If the local runtime
itself refuses a request for context before any output, the core re-consults
ARC with that fact instead of truncating.

With the default profile, the core measures each request before it is sent.
For Qwen3.8-27B the count is exact. For another model named in
`hybrid.local_registry_model` it is an upper bound worked out from the
request's size, so a request close to the local window can go to cloud even
though it would have fit. If Raydock cannot read which model the runtime
serves (LM Studio) and `hybrid.local_registry_model` is not set, it assumes
Qwen3.8-27B and counts as if Qwen were running. A request that does not fit
goes to a cloud model, as described above. A profile file with a qualified
llama.cpp prompt counter (see below) counts every request with the runtime's
own tokenizer, so the count is exact for any model.

For llama.cpp:

- Set `--ctx-size` to the profile's `context_window` and use `--parallel 1`.
  The window is stated as `configured` until an adapter observes the real
  per-slot allocation.
- Consider `--cache-ram 0`. The host prompt cache otherwise grows memory by up
  to 8 GiB over a long session.

## Events from an unmodified harness

The core infers what each request is from the harness's own wire traffic:

- **Task start:** a request with no assistant message yet.
- **Tool result or typed user turn:** read from the trailing message. Injected
  reminders and harness-authored text do not count as user words.
- **Side calls:** the harness's auxiliary requests, such as compaction summaries,
  titles and prompt suggestions. They never advance the session. Claude Code's
  auto-mode safety classifier is the exception: it always goes to cloud,
  because a local model's answer it cannot parse makes Claude Code ask you to
  approve every action. It uses Claude Haiku 5.5 through your router; set
  `"hybrid": {"classifier_model": "<router model id>"}` in `settings.json`, or
  pass `--classifier-model` to `start`, to use another model.
- **Compaction:** detected by comparing each request with the previous one,
  including Pi's retained tail.

No harness extension or patch is needed.

## Advanced: a profile file

`raydock hybrid start --config /absolute/path/to/hybrid.json` uses a qualified
execution-core profile instead of the default. The `hybrid.routing` overrides
still apply. The profile is passed to the core as written, so one from before
the Routes API V1 contract (a `routing.exclusive` member, `value_usd` instead
of `v_value`, or a precomputed `price_sha256`) is refused by the core at
startup; the refusal names the replacement in the runtime log. Use one to:

- pin a policy package and its actions;
- choose other cloud targets;
- qualify a llama.cpp prompt counter (model file, build and template pins) for
  exact local token counts and capacity facts.

The fields are described in [ARC hybrid execution](hybrid-routing.md).

## Troubleshooting

- `raydock hybrid status` shows readiness, process identity and the advertised
  model capabilities.
- Refusals come back as errors in the harness:
  - `policy_preset_not_admitted`: the cell does not serve that preset.
  - `policy_schedule_not_admitted`: the package does not support that schedule
    class.
  - `execution_no_feasible_target`: nothing that fits was offered.
- The runtime log is `runtime.log` in the state directory, `~/.rayline/hybrid`
  by default. It records each decision's receipt, the capacity facts, and the
  executed model, without prompts.
