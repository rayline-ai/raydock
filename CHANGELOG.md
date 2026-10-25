# Changelog

User-facing changes in each Rayline release. Install commands and signed
assets are listed on each [GitHub release](https://github.com/rayline-ai/rayline/releases).

## 0.2.13 (unreleased)

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
- Rolling back works: `rayline update --version 0.2.12` installs a release
  whose signed checksums list no core and skips the core. From 0.2.13 on, the
  core is required, and the update fails closed if it is missing (hardening PR).

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
  (hardening PR).
- Tokio is updated to 1.53.3 for RUSTSEC-2026-0386
  ([#121](https://github.com/rayline-ai/rayline/pull/121)).

### Known issues

Hybrid issues are tracked in [#123](https://github.com/rayline-ai/rayline/issues/123).

- Only Pi receives the local model's limits in `rayline hybrid run`. Other
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
