# Changelog

User-facing changes in each Rayline release. Install commands and signed
assets are listed on each [GitHub release](https://github.com/rayline-ai/raydock/releases).

## 0.3.0 (unreleased)

### Changed

- **Rayline Local is now Raydock.** Use `raydock` for new commands; `rayline`
  remains a supported alias. Configuration, credentials, environment variables
  and update channels keep their paths and names. The source repository is now
  [rayline-ai/raydock](https://github.com/rayline-ai/raydock).
- **Harness launches run on ARC by default** instead of the Legacy Rules
  Router. Hosted `raydock claude`, `raydock codex` (default Rc-Rc) and the
  `opencode`, `pi`, `omp`, `hermes` and `openclaw` launchers give the main agent
  `arc-sage-1.0`, and subagents and background tasks `arc-swift-1.0` where the
  harness has a separate setting for them (Claude Code's
  `CLAUDE_CODE_SUBAGENT_MODEL` and `ANTHROPIC_DEFAULT_HAIKU_MODEL`, OpenCode's
  `small_model`, omp's `smol` role, Codex's `subagent` route). The Legacy Rules
  Router stays available by explicit choice: `--model rayline-router`, the
  harness's own model flag or picker (every generated config lists it;
  `raydock codex --model rayline-router` too), or a route model in
  `router.json`. Choices you made are kept: an explicit model,
  your own Claude Code model variables, `--config` routes, and the model pi
  and omp saved (Hermes and OpenClaw configs are rewritten each launch, as
  before, so pick Legacy there per run). Generated defaults that were never edited (the 0.2.x
  `router.json`, pi `settings.json` and omp `config.yml`) move to ARC once.
  Local and hybrid sessions are unchanged.
- A cloud-only `raydock claude --config` launches the main agent with the model
  its `routes.main` names and, with `--route all`, subagents with its
  `routes.subagent` model.
- Under `--route subagents`, a request naming an ARC model (`arc-*`) always
  goes to the router, like `rayline-router`: Anthropic serves neither.
- **`raydock openclaw` speaks Chat Completions and keys its own episode.**
  The generated `rayline` provider is now `openai-completions` (OpenClaw's
  default protocol) with `compat.sendSessionAffinityHeaders`, so OpenClaw
  sends its session id as
  `x-session-affinity` and a `--session-id` resume keeps its routing episode.
  The launcher no longer sends a per-launch `x-conversation-id` for OpenClaw;
  `RAYLINE_HARNESS_CONV_ID_FALLBACK=1` still forces it. OpenAI-style hosted
  tools (`web_search`) are not carried on Chat Completions; function tools
  are.
- **Harness launchers rely on the router's session keying.**
  `raydock opencode|pi|omp|hermes|openclaw` no longer send a per-launch
  `x-conversation-id` unless `RAYLINE_HARNESS_CONV_ID_FALLBACK=1` forces it.
  The router keys episodes on each harness's own session id on dev and prod
  (prod since 2026-10-23), so `--resume` / `-c` in a new launch keeps the
  episode. Hermes on Messages sends no session id, and the router now derives
  its episode from the conversation instead; with the per-launch id, a
  `delegate_task` subagent's turns and the title side call were all keyed
  into the parent's episode. Under `raydock hybrid run` the Hermes Messages
  provider keeps the per-launch id, since the execution core refuses a request
  with no identity.

## 0.2.14 (2026-10-27)

### Fixed

- **Hybrid routing checks the local model is Qwen3.8-27B.** `rayline hybrid
  start` reads what the runtime has loaded (llama.cpp `/props`, Ollama
  `/api/show`); another model is refused for local routing and the session
  runs cloud only, with an error naming what was found. Runtimes that do not
  report it get a warning. Release archives now include `NOTICE`, which
  attributes the embedded Qwen3.8-27B tokenizer data (Apache-2.0).
- **`rayline hybrid run opencode` works with OpenCode 2.** Every turn was
  refused with "output allowance exceeds the admitted deployment limit":
  every hybrid harness (OpenCode, omp, OpenClaw, Hermes, Claude Code, Codex)
  now advertises the runtime's admitted output cap and text-only input, as
  Pi already did (#123). OpenCode 2 launches (`rayline opencode` and
  `rayline hybrid run opencode`) add `--standalone`, since OpenCode's shared
  background service never saw Rayline's configuration, and the OpenCode
  config drops the `attachment` and `reasoning` keys OpenCode 2 rejects.
  Hybrid refuses OpenCode `-c` / `--session` / `--fork` like other resumes.
- **Clearer hybrid errors.** `rayline hybrid start` checks Decisions once
  before reporting ready (a rejected key names the `auth login` command; a
  starting preview cell is waited for). During a session, a Decisions key
  refusal names the same command, and a Decisions outage is retried once,
  then reported as retryable (HTTP 503).
- **`rayline local test`** checks Chat Completions, the API hybrid routing
  uses, and reports Anthropic Messages separately.

### Changed

- **Hybrid context.** Harnesses are told the window is 1,000,000 tokens; the
  default cloud targets (Opus 5.5, Sonnet 5.5) carry their 1M window; the
  local window defaults to 98,304 tokens (`hybrid.local_context_window`,
  `--local-context-window`), capped by a llama.cpp runtime's loaded window.
  Ollama and MLX no longer need `--local-context-window`, but must be loaded
  with that window. Hermes uses its Responses provider under hybrid.

## 0.2.13 (2026-10-26)

### New

- **`rayline hybrid` (preview).** Routes each turn to a local Qwen 3.8 27B on
  your machine or to a cloud model through the Rayline router, and your coding
  harness runs unmodified. Preview: development router only, until production
  support lands. macOS 13+ or Linux
  ([#120](https://github.com/rayline-ai/raydock/pull/120),
  [#122](https://github.com/rayline-ai/raydock/pull/122)). Setup, with the
  development environment configured in `~/.config/rayline/settings.json`:
  1. `rayline --env dev auth login`
  2. `rayline local custom --url http://127.0.0.1:8080/v1 --model qwen3.8-27b`
     (your own llama.cpp, Ollama or MLX server)
  3. `rayline hybrid start --env dev --cell pf-5505cbf8-v2`, then
     `rayline hybrid run pi` (also `claude`, `codex`, `opencode`, `omp`,
     `hermes`, `openclaw`)

  Ollama and MLX also need `--local-context-window <tokens>` on `start`.
  Start a fresh harness session after every `start`; resuming a saved session
  is not supported. See [docs/hybrid-local-setup.md](docs/hybrid-local-setup.md).
- **Hybrid routing overrides.** An optional `hybrid.routing` block in
  `~/.config/rayline/settings.json` sets `policy` (`utility` or
  `prefer_local`), `min_p_pass` (strictly between 0 and 1, `prefer_local`
  only), `schedule`, `value_usd` (0.1 to 5) and `local_price` (default profile
  only). `prefer_local` needs a profile file with a qualified llama.cpp prompt
  counter. Unknown keys are refused. Details in
  [docs/hybrid-local-setup.md](docs/hybrid-local-setup.md#the-optional-override-block).
- **`rayline claude-desktop` sets up and launches Claude Desktop in one
  command (macOS).** It selects a **Rayline ARC** profile and opens the app.
  This switches the whole app off your subscription, with Chat and Cowork off,
  until you run `rayline claude-desktop off`
  ([#121](https://github.com/rayline-ai/raydock/pull/121)).

### Changed

- The installers (curl, Homebrew, PowerShell) and `rayline update` now also
  install `rayline-core`, the hybrid execution core. If you update from an
  older Rayline and the core is still missing, run `rayline update` again.
- Rolling back works: `rayline update --version 0.2.12+ee1afb00b6cd` installs
  a release whose signed checksums list no core, skips the core and removes the
  installed one. Pass the full version from the release notes; a bare
  `--version 0.2.12` says so. From 0.2.13 on, the core is required, and the
  update fails closed if it is missing
  ([#125](https://github.com/rayline-ai/raydock/pull/125)).

### Fixed

- Claude Code in proxy mode keeps the full conversation history. Rayline turns
  off Claude Code's message threads by default (your own setting is kept) and
  refuses router-bound thread continuations locally, so Claude Code resends the
  full history ([#119](https://github.com/rayline-ai/raydock/pull/119)).

### Security

- Hybrid writes `core-config.json` owner-only.
- Your saved Rayline key goes only to HTTPS endpoints on the signed-in
  environment's router host.
- Hybrid decision requests go only to the Rayline router or to loopback
  ([#125](https://github.com/rayline-ai/raydock/pull/125)).
- Tokio is updated to 1.53.3 for RUSTSEC-2026-0386
  ([#121](https://github.com/rayline-ai/raydock/pull/121)).

### Known issues

Hybrid issues are tracked in [#123](https://github.com/rayline-ai/raydock/issues/123).

- In `rayline hybrid run`, Pi gets the local model's output and input limits,
  and the largest cloud context window (200,000 tokens with the default
  profile), so turns that outgrow the local model hand off to cloud. Other
  harnesses keep their usual limits and may send requests the core rejects.
- Ctrl-C during `rayline hybrid start` can leave the runtime running. Run
  `rayline hybrid stop` once it is ready; if it never gets ready, stop the
  process whose `pid` is in `~/.rayline/hybrid/supervisor.json`, wait for it
  to exit, then run `rayline hybrid stop`.
- Smaller hybrid issues: a transient connection error can stop the gateway;
  `--state-dir` makes an existing directory owner-only; and with the core
  missing, `rayline update --check` reports an error even when up to date.
- `rayline claude-desktop --prefixed-ids` forwards `rayline/arc-sage` and
  `rayline/arc-swift`, which the hosted router refuses (400). The default bare
  ids work: the picker's `arc-sage` and `arc-swift` map to the ARC tiers
  `arc-sage-1.0` and `arc-swift-1.0`.

Release notes for 0.2.12 and earlier are on
[GitHub Releases](https://github.com/rayline-ai/raydock/releases).
