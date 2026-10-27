# Changelog

User-facing changes in each Rayline release. Install commands and signed
assets are listed on each [GitHub release](https://github.com/rayline-ai/rayline/releases).

## Unreleased

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
  ([#120](https://github.com/rayline-ai/rayline/pull/120),
  [#122](https://github.com/rayline-ai/rayline/pull/122)). Setup, with the
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
  ([#121](https://github.com/rayline-ai/rayline/pull/121)).

### Changed

- The installers (curl, Homebrew, PowerShell) and `rayline update` now also
  install `rayline-core`, the hybrid execution core. If you update from an
  older Rayline and the core is still missing, run `rayline update` again.
- Rolling back works: `rayline update --version 0.2.12+ee1afb00b6cd` installs
  a release whose signed checksums list no core, skips the core and removes the
  installed one. Pass the full version from the release notes; a bare
  `--version 0.2.12` says so. From 0.2.13 on, the core is required, and the
  update fails closed if it is missing
  ([#125](https://github.com/rayline-ai/rayline/pull/125)).

### Fixed

- Claude Code in proxy mode keeps the full conversation history. Rayline turns
  off Claude Code's message threads by default (your own setting is kept) and
  refuses router-bound thread continuations locally, so Claude Code resends the
  full history ([#119](https://github.com/rayline-ai/rayline/pull/119)).

### Security

- Hybrid writes `core-config.json` owner-only.
- Your saved Rayline key goes only to HTTPS endpoints on the signed-in
  environment's router host.
- Hybrid decision requests go only to the Rayline router or to loopback
  ([#125](https://github.com/rayline-ai/rayline/pull/125)).
- Tokio is updated to 1.53.3 for RUSTSEC-2026-0386
  ([#121](https://github.com/rayline-ai/rayline/pull/121)).

### Known issues

Hybrid issues are tracked in [#123](https://github.com/rayline-ai/rayline/issues/123).

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
[GitHub Releases](https://github.com/rayline-ai/rayline/releases).
