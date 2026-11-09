# Changelog

User-facing changes in each Rayline release. Install commands and signed
assets are listed on each [GitHub release](https://github.com/rayline-ai/raydock/releases).

## 0.4.3 (2026-11-09)

### New

- **Hybrid mode runs on any local model, not just Qwen3.8-27B.** Set
  `hybrid.local_registry_model` in `~/.config/rayline/settings.json` to the
  Rayline model ID of the model you serve, such as `"z-ai/glm-5.3-flash"`.
  ARC then predicts for the model that actually runs. The value must have the
  form `provider/model`; the execution core refuses a model its registry
  doesn't know, and the error names the setting. It can't be combined with
  `--config`, since a profile file sets its own model.
- **`prefer_local` works with any local model.** The bundled execution core
  (`rayline-core`, now pinned to `dbde74c0`) counts Qwen3.8-27B prompts exactly
  and bounds other models' text prompts by their byte size (`caller_bounded`),
  so ARC can keep a turn local only when it fits the window. It no longer needs
  a llama.cpp prompt counter. This needs the production router from
  2026-11-09 or later.
- **Auto-mode safety checks go to the cloud.** Claude Code's auto-mode
  classifier calls are sent to Claude Haiku 5.5 through the router instead of
  the local model. Change the model with `hybrid.classifier_model` or
  `--classifier-model`.

### Fixed

- **Hybrid uses the local runtime's own context window.** With no
  `--local-context-window` or `hybrid.local_context_window`, `hybrid start`
  now uses the window the runtime reports (llama.cpp `/props`, or
  `/v1/models` from vLLM or TensorFold, up to 1,000,000) instead of 98,304.
  Before, Claude Code on a model other than Qwen3.8-27B never ran locally:
  its opening request alone is about 110,000 tokens. Ollama and MLX, which
  report no window, keep the 98,304 default; a configured window still wins,
  lowered to a smaller loaded one.
- **Hybrid reads the context window from `/v1/models`.** When a server has no
  llama.cpp `/props` endpoint (vLLM, TensorFold), `hybrid start` reads
  `max_model_len` or `context_length` from the served model's entry. As with
  llama.cpp, this can only lower the configured window; a larger loaded window
  prints how to raise the setting.
- **A turn too long for a vLLM or TensorFold server moves to the cloud.** When
  the server states its window in `/v1/models`, the local deployment is marked
  as refusing over-window prompts, and the core asks ARC again after such a
  refusal and moves the turn to a model that fits. llama.cpp (which may
  truncate with `--context-shift`), Ollama and MLX keep the default.
- **Each cloud target declares its own model's output ceiling.** Both Claude
  5.5 cloud deployments now allow 128,000 output tokens instead of a shared
  32,000. Override per model with `hybrid.cloud_output_tokens`.
- **Every deployment lowers an over-cap output allowance.** A request asking
  for more output than a cloud deployment's ceiling is lowered to it instead of
  refused, as the local deployment already did; the stated and sent figures are
  logged. A turn whose thinking budget reaches the ceiling is still refused.

### Note

Hybrid routing decisions send the conversation to Rayline's router: only local
inference stays on your machine. For fully local use, run
`raydock claude --local` with a fully local config. See
`docs/hybrid-local-setup.md`, "What leaves your machine".

## 0.4.2 (2026-11-07)

### New

- **Hybrid mode runs on Windows.** `raydock hybrid start`, `status`, `stop`
  and `run` no longer refuse Windows. A second `hybrid start` waits on a real
  start lock, the hybrid state directory and its token, ready, supervisor,
  config and log files are readable by the current user only, and `rld` runs
  detached, so closing the console that started it no longer stops it.
  `hybrid stop` shuts `rld` and the execution core down gracefully and removes
  their token and ready files; the timeout and hard-kill fallback are
  unchanged. `hybrid run codex` and `hybrid run pi` find npm's `.cmd`
  launchers. Windows CI now runs the hybrid lifecycle tests. macOS and Linux
  behaviour is unchanged.
- **`raydock router start` runs `rld` detached on Windows, as hybrid does.**
  The router and proxy that `router start`, `raydock claude --local` and the
  other harness launches start no longer hold the caller's standard handles
  or live in its console, so `raydock router start | cat` and
  `$(raydock router start)` return as soon as start finishes, and closing the
  console that ran `router start` no longer stops `rld`. macOS and Linux
  behaviour is unchanged.

### Fixed

- **`raydock hybrid run claude` works on cloud routes.** The execution core
  (`rayline-core`) is pinned to a newer source commit whose `model_pin`
  Messages transport forwards the `system` rows Claude Code 2.x sends inside
  `messages` and no longer refuses a request body that merely contains the
  string `"xhigh"`. Before, every Claude Code 2.x turn on a cloud route failed
  with `400 model_pin Messages cannot preserve in-array system or developer
  messages`. The root effort fields are still stripped before dispatch, and
  in-array `developer` rows are still refused (see Known issues).
- **Hybrid tells Claude Code the gateway's context window.** `raydock hybrid
  run claude` now sets `CLAUDE_CODE_MAX_CONTEXT_TOKENS` to the window the
  gateway advertises, so Claude Code no longer assumes 200k for
  `rayline-auto`, auto-compacts early or warns about an unrecognized model on
  every launch. Other harnesses already got their window through their config
  files.
- **The local deployment lowers an over-cap output allowance.** A harness that
  asks for more output than the local deployment's cap (a quarter of the local
  window, 24,576 tokens at the default 96k), such as Hermes with its fixed
  128,000, has its local turns lowered to the cap instead of refused with
  `output allowance exceeds the admitted deployment limit`. Cloud deployments
  still refuse an allowance over their ceiling.
- **Streamed Responses turns on an Anthropic route record their cache-read
  tokens.** When a Codex `/v1/responses` turn is served through an
  `anthropic_messages` endpoint and both sides stream, the local router now
  closes the metrics row with the cached part of the prompt as well as the
  prompt and output counts, so `raydock top`'s cache column covers the shape an
  interactive turn normally takes. The row also gains the prompt total the
  cache hit rate is measured against, so that column reads as a rate rather
  than a blank. Those counts were dropped for stream-to-stream turns only; the
  buffered shapes and the Chat Completions and Messages paths already recorded
  them. An upstream that reports no cache tokens still leaves the count unset
  rather than reporting a zero. Response bodies and the `usage` the client
  receives are unchanged.
- **Python 3.13+ clients trust the proxy's certificates.** Proxy leaf
  certificates now carry an Authority Key Identifier, filled from the CA's own
  Subject Key Identifier, so it matches CAs already on disk. Python 3.13+
  rejects a leaf without one (`CERTIFICATE_VERIFY_FAILED: Missing Authority
  Key Identifier`), so any Python client that trusted the proxy CA failed its
  TLS handshake.

### Changed

- **Responses cost details use the upstream spelling.** The execution core
  now names the provider charge's `cost_details` halves
  `upstream_inference_input_cost` and `upstream_inference_output_cost` on
  Responses (Chat Completions keeps the prompt/completions spelling). Raydock
  reads either spelling.

### Docs

- The routing-modes examples follow the `rayline-router` removal: configs pin
  current model ids, the retired `Rcl` modes are documented as behaving like
  `Rc`, and a Hybrid mode section links the setup guide.
- The SDK examples run as shipped: `local/python` builds its client with the
  SDK's own HTTP client, the local examples ask a question a raw API call can
  answer, and the examples allow 2048 output tokens so reasoning models print
  an answer.
- The hybrid setup guide states that Qwen3.8-27B needs a machine with at
  least 32 GB of memory, and that a runtime which reports no model name gets a
  warning and is assumed to be Qwen3.8-27B.

### Known issues

- **Codex through hybrid still fails on cloud `model_pin` Messages routes.**
  The execution core refuses the in-array `developer` messages Codex sends on
  that transport, so `raydock hybrid run codex` turns that land on such a
  route fail. Local turns and Claude Code are not affected.

## 0.4.1 (2026-11-05)

### Changed

- **The Legacy Rules Router is removed (breaking).** The hosted router no
  longer serves `rayline-router`, and Raydock no longer offers or sends it:
  the generated `router.json` lists only `arc-sage-1.0` and `arc-swift-1.0`,
  `raydock codex` no longer pins `rayline-router` to the Legacy model, the
  `--model rayline-router` help text and the "Legacy Rules Router" model label
  are gone, and a may-local `rayline-cloud` route without a `model` is refused
  at `router start` instead of being filled with `rayline-router`. A file or
  setting of yours that still names `rayline-router` for the hosted router is
  not rewritten: its turns fail with the router's 404 until you change them to
  `arc-sage-1.0` or `arc-swift-1.0`. That includes the default `router.json`
  an 0.2.x release generated, which Raydock no longer upgrades. The omp and pi
  migrations of saved Legacy picks to ARC stay.
- **The on-device virtual model is now `rayline-auto` (breaking).** The local
  router, the proxy and the hybrid execution core used `rayline-router` as
  their "route by `routes.main`" model; they now use `rayline-auto`, and no
  longer treat `rayline-router` specially. On the local router it is now an
  unknown model name like any other, so it follows `routes.main`: `raydock
  codex --model rayline-router` and an exported
  `ANTHROPIC_MODEL=rayline-router` on the local plane run on the config's main
  route. The local router and the proxy no longer rewrite the retired
  `claude-rayline-router-*` and `rayline-router-balanced` aliases either.

## 0.4.0 (2026-11-05)

### New

- **Each forwarded request records the cloud router's id.** When Rayline Local
  forwards a request to the router, the id the router gives it
  (`x-rayline-route-id`) is kept with the request: the route log
  line is followed by `… route upstream request_id=… upstream_request_id=rt_…`,
  the metrics snapshot's request rows gain `upstream_request_id`, `raydock top`
  shows it in an `UPSTREAM` column on its wide layout, and the response carries
  it back as `x-rayline-upstream-request-id`. Route log lines now also name the
  local `request_id`, so the two lines can be matched. A request answered on the
  device without a cloud routing hop has no upstream id and shows `-`; one the
  router redirected to the device keeps the router's id.

### Changed

- **V is now `v_value` (breaking).** A profile file sets V under
  `decisions.routing.v_value`, which hybrid routing sends to the Routes API
  as `routing.v_value`, the API's new name for the member. Hybrid routing
  needs this release from the moment the Routes API stops accepting
  `value_usd`.
- **The default hybrid profile decides with ARC Swift and sends no V
  (breaking).** It asks the router for decisions with model `arc-swift-1.0`
  instead of `rayline-router` with a V of 0.5. Swift decides at its own fixed
  V and picks cheap, so it uses the local model most. The `hybrid.routing`
  override in `settings.json` no longer takes a V: `v_value` and `value_usd`
  are refused at `start` (`settings hybrid.routing.v_value is not
  supported`); remove the key. A profile file that wants its own V sets
  `decisions.routing.v_value` with model `arc-1.0`.
- **Hybrid routing is no longer a development-router preview.** A production
  `raydock hybrid start` no longer prints the preview note: hybrid uses the
  production router by default, and `--env dev` selects the development one.
  The hybrid docs now say what a turn costs: every turn bills one ARC decision,
  local turns included, and a cloud turn also bills the provider's
  pass-through price.

### Fixed

- **Hybrid routing decides again on the hosted router.** Since the router moved
  to the Routes API V1 contract (2026-11-04), every `raydock hybrid` decision
  was refused with `routing has unsupported member(s): exclusive` and the turn
  failed. The default profile no longer sends `exclusive` or a precomputed
  `price_sha256`, and the bundled execution core checks the router's V1 price
  acknowledgement. A profile file from before V1 (with `routing.exclusive`,
  `value_usd` or a precomputed `price_sha256`) is refused by the core at
  startup, and the runtime log names the replacement.
- **Chat Completions requests show in `raydock top`.** The local router's
  `/v1/chat/completions` listener now records the same request lifecycle as
  the Messages and Responses listeners: the route when it is decided, the
  first token, the Chat `usage` counts (prompt, completion, cached) and the
  completion, or the error for an upstream failure, a cut-off stream or a
  refused request. Cloud requests from OpenAI-compatible harnesses (OpenClaw,
  Roomote, Cline in OpenAI mode) were missing from `raydock top` and its
  totals; requests served on-device already appeared and are unchanged. On a
  passthrough route, a streaming request that did not ask for usage now asks
  the upstream for it so the row carries real token counts; that extra usage
  chunk is withheld from the client, whose stream is unchanged.
- **Non-streaming Responses turns on an Anthropic route no longer stay active
  in `raydock top`.** When a Codex `/v1/responses` turn is served through an
  `anthropic_messages` endpoint and the translation is not stream-to-stream
  (a `stream: false` turn such as Codex compaction, or a `stream: true` turn
  the upstream answered in one piece), the local router now closes the
  request's metrics row with the upstream's token counts (whole prompt,
  output, cached part) and the selected model. Those turns were left in
  `active` with state `routed` forever, so `raydock top` listed them as
  never finishing, `active_requests` was wrong and the completed and token
  totals missed them. A buffered upstream stream that reported an error or
  stopped early is recorded as an error, not a completion. Response bodies
  are unchanged. A Responses turn served on-device is recorded by the
  adapter, as before, and no longer also leaves a bare duplicate row.

## 0.3.1 (2026-11-02)

### Changed

- Hosted `raydock claude` now picks the model of every Claude Code subagent and
  background task, ignoring the models Claude Code's own agent definitions
  name. Without `--model`, they run on `arc-swift-1.0`; with `--model X`
  (before or after `--`, or `ANTHROPIC_MODEL`) and `--route all`, they run on X
  too, instead of only the main agent. A cloud-only `--config` uses its
  `routes.subagent`, else the main model. Raydock sets
  `CLAUDE_CODE_SUBAGENT_MODEL_FORCE=1` for this, so Claude Code's Explore and
  Plan agents no longer inherit the main agent's model, and a model a custom
  agent names, or the main agent passes when spawning, is overridden too. Under
  `--route subagents`, subagents stay on `arc-swift-1.0` whatever `--model`
  says. Your own `CLAUDE_CODE_SUBAGENT_MODEL` keeps Claude Code's precedence.
  The hosted router still replaces a `claude-*` model on subagent and
  background turns.
- Under `--route subagents` with the hosted router, a routed subagent that
  names a Claude model now runs on its `--config` route's model
  (`routes.subagents.<type>`, then `routes.subagent`), else `arc-swift-1.0`,
  instead of always on the Legacy Rules Router (`rayline-router`).
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

### Fixed

- **omp's ARC models no longer appear free.** `raydock omp` supplies Sage and
  Swift's fixed input, output, cache-read and cache-write rates (effective card
  26, 2026-10-27). Hosted ARC entries advertise text input and leave reasoning
  effort to ARC instead of exposing unsupported client thinking controls.
  Existing context/output budgets, saved role choices and hybrid capabilities
  are unchanged. Hybrid prices remain variable; omp versions that label missing
  prices as free must not be used as billing evidence. The
  [omp onboarding guide](docs/getting-started.md#oh-my-pi-omp) now starts with
  the launcher, explains isolation and resuming, and includes a no-inference
  metadata check.
- **omp model labels omit deployment environments:** the picker and composer
  show `ARC Sage` and `ARC Swift` without `(prod)` or other environment
  suffixes. The launch banner still identifies the environment.
- **Hosted omp no longer lists the Legacy Rules Router.** Its model picker
  offers only ARC Sage and ARC Swift. Hybrid retains the execution core's
  active virtual model, which happens to use the same `rayline-router` wire ID.
- **Existing omp sessions survive the Legacy model's removal.** Hosted
  launches migrate retired role and session selections to ARC while preserving
  other settings and historical messages, usage and served-model metadata.
  Journal modification times are retained so `--continue` keeps selecting the
  correct conversation. Migration cooperates with omp's settings lock and
  refuses active writers, concurrent changes and unsafe state instead of
  overwriting it; hybrid state is never migrated by this cutover.
- **Supply-chain checks recognize the reviewed Boost license** for the pinned
  `xxhash-rust` dependency used by omp's native lock compatibility. The exception
  is crate/version-scoped, and distributed notices include the license text.

## 0.3.0 (2026-10-28)

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
