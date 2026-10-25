# Hybrid routing with a local model: setup

Hybrid routing keeps easy work on a model running on your machine and sends
harder work, or work that no longer fits the local context window, to a cloud
model. ARC, the hosted policy, makes each decision. The `rayline-core`
executor runs it, and your coding harness runs unmodified.

This page covers the user setup. For the process contract, configuration
reference and validation scripts, see [ARC hybrid execution](hybrid-routing.md).

## Requirements

- macOS 13 or later, or Linux, with a Rayline release installed. The release
  includes `rayline`, `rld` and `rayline-core`.
- A local inference server with your Qwen model: llama.cpp, Ollama or MLX.
  Rayline does not start, download or stop it.
- Nothing else. A profile file is optional (see [Advanced: a profile file](#advanced-a-profile-file)).

## Three steps

1. **Sign in.** Run this once:

   ```bash
   rayline auth login
   ```

   The saved Rayline API key is used only for an HTTPS endpoint on the same host
   as that environment's router. An API key variable set in the launch
   environment (for example `RAYLINE_ROUTER_API_KEY`) always wins. Startup
   prints only the variable's name.

2. **Point Rayline at your local model.**

   ```bash
   rayline local custom --url http://127.0.0.1:11434/v1 --model qwen3.8:27b
   ```

   This is the local endpoint hybrid routing uses. ARC treats it as Qwen 3.8
   27B, its local model.

3. **Start and connect a harness.**

   ```bash
   rayline hybrid start
   rayline hybrid run pi -- "Fix the failing test in parser.py"
   rayline hybrid status
   rayline hybrid stop
   ```

   With no profile file, `start` builds the default profile:
   - Decisions come from your signed-in router's `rayline-router` model.
   - Your `local_model` is the local target.
   - Claude Opus 5.5 and Sonnet 5.5, through the same router, are the cloud
     targets.
   - The local context window is read from llama.cpp. For Ollama or MLX, pass
     `--local-context-window <tokens>`.
   - Add `--env dev` to use a signed-in development environment.

   `start` prints the profile, the routing policy and the local model it bound.
   `run` also supports `claude`, `codex`, `opencode`, `omp`, `hermes` and
   `openclaw`. Start a fresh harness session after every `start`.

## Routing policy

With no override, Rayline Local sends the **hybrid default** preset,
`arc-policy.hybrid-default.v1`, on every decision. Under this preset:

- **The model** may change at cache boundaries: task start, a subagent start, a
  compaction, or a typed user turn whose cache is cold.
- **The thinking level** may change on every turn.
- **The capacity handoff** is always allowed (below).

Easy work stays on device. Harder work and context spills go to the cloud.

### The optional override block

Every key in this block, in `~/.config/rayline/settings.json`, is optional:

```json
{"hybrid": {"routing": {"policy": "prefer_local",
                        "min_p_pass": 0.65,
                        "schedule": {"model": ["cache_boundaries", "user_messages"]},
                        "value_usd": 1.0}}}
```

| Key | Meaning |
| --- | --- |
| `policy` | `utility` selects the hybrid default preset. `prefer_local` selects `arc-policy.prefer-local.v1`: the local model is used whenever ARC predicts it will pass (`p_pass >= min_p_pass`), and utility selection over the cloud targets is used otherwise. |
| `min_p_pass` | The prefer-local threshold, in (0, 1). The service default is 0.65. It is valid only with `prefer_local`. |
| `schedule` | When the model (`model`) and thinking level (`thinking`) may change. Each is a set of classes: `cache_boundaries`, `user_messages` (any typed user turn) or `all_turns`. The service refuses a class its package does not support (`policy_schedule_not_admitted`). |
| `value_usd` | Your V: what a successful task is worth to you in dollars, between 0.1 and 5. A higher V favours more capable, more expensive models. It selects ARC's custom result. |

Unknown keys are refused. A profile pinned to the older `current` or
`execution_context_v1` Decisions mode is passed through unchanged, and an
override block is refused for it.

## The capacity handoff

The local window is the profile's `context_window`. Before each request, the
core counts the exact prompt the local model would receive with the runtime's
own tokenizer. The count covers tools, history and private instructions. The
core adds the output limit, then tells ARC whether the request fits.

When the conversation grows past the local window, for example during a long
tool loop with no compaction, ARC hands the turn to a cloud target. The receipt
reads `decided_by: capacity_handoff`, and the local target is listed as
excluded for `context_capacity`. The conversation continues on the cloud model.
No tool is replayed, and nothing is reselected mid-stream. If the local runtime
itself refuses a request for context before any output, the core re-consults
ARC with that fact instead of truncating.

With the default profile the core cannot count tokens in advance, so the local
target's capacity is reported as unknown. It still hands off when the runtime
refuses a request for context (llama.cpp reports this before any output). A
profile with a qualified llama.cpp prompt counter (see below) counts every
request up front, so the handoff happens before dispatch.

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
  titles and prompt suggestions. They never advance the session.
- **Compaction:** detected by comparing each request with the previous one,
  including Pi's retained tail.

No harness extension or patch is needed.

## Advanced: a profile file

`rayline hybrid start --config /absolute/path/to/hybrid.json` uses a qualified
execution-core profile instead of the default. The `hybrid.routing` overrides
still apply. Use one to:

- pin a policy package and its actions;
- choose other cloud targets;
- qualify a llama.cpp prompt counter (model file, build and template pins) for
  exact local token counts and capacity facts.

The fields are described in [ARC hybrid execution](hybrid-routing.md).

## Troubleshooting

- `rayline hybrid status` shows readiness, process identity and the advertised
  model capabilities.
- Refusals come back as errors in the harness:
  - `policy_preset_not_admitted`: the cell does not serve that preset.
  - `policy_schedule_not_admitted`: the package does not support that schedule
    class.
  - `execution_no_feasible_target`: nothing that fits was offered.
- The runtime log is `runtime.log` in the state directory, `~/.rayline/hybrid`
  by default. It records each decision's receipt, the capacity facts, and the
  executed model, without prompts.
