# ARC hybrid execution

`rayline hybrid` runs a standalone Go execution core behind an authenticated
loopback gateway managed by `rld`. The core owns ARC decisions, wire-format
conversion, thinking controls, private append, and execution context. The Rust
gateway streams the original request and response bytes without entering the
static router or adapter.

This is an explicit mode. `rayline claude --local` continues to mean auth-free
local static routing.

## Bring your own inference server

Start and configure your Qwen model using your preferred server, such as Ollama,
MLX, or llama.cpp. Its endpoint, served model ID, protocol, runtime identity and
qualified thinking-control profile belong in the execution-core configuration.
The model's display name alone does not establish template or thinking-control
compatibility. `rld hybrid` does not download a model, start an inference server,
change its settings, or stop it.

Use the execution core's configuration and policy-package tooling to bind ARC
actions to that local deployment and to named cloud deployments. Cloud requests
must name the selected worker rather than submit a second routing request. The
configuration contains credential environment-variable names, never keys. The
core validates the policy-package and thinking-registry hashes at startup.

For hosted Decisions and Rayline cloud inference, the core reads the Rayline
API key from the environment variable each endpoint names (normally
`RAYLINE_ROUTER_API_KEY`). An explicit launch-environment value always wins.
Otherwise `start` supplies the key saved by `rayline auth login` only for an
HTTPS endpoint on the same host as that signed-in environment's router (for
example the dev key for `https://api-dev.rayline.ai`); it never offers a saved
key to another host and prints only the variable name.
The core rejects missing credential environment references before startup. The
launcher withholds every configured credential environment variable from the
harness, including custom Decisions and inference key names. Readiness metadata
contains only those names; the harness receives its separate gateway token.
A local Decisions
provider can be selected through the core configuration when its contract is
supported by the core; Rust performs no policy selection.

**Decisions endpoint allowlist.** Decisions requests carry the session's
execution context and the router API key, so `decisions.url` must be either
HTTPS on the exact host and port of a Rayline router (`api.rayline.ai`,
`api-dev.rayline.ai`, or the `router_url` of an environment configured in
settings.json), or a loopback address (`127.0.0.0/8`, `::1`, `localhost`, over
HTTP or HTTPS) for tests and a local Decisions service. Anything else, such as
a direct `*.modal.run` URL, is refused by `start` with an error that names only
the host and port. `rld hybrid` repeats the check on the file it gives the core,
so a hand-edited `core-config.json` cannot widen it, and refuses a
configuration without `decisions.url`, with duplicate JSON keys, or with a key
that Go's case-insensitive decoder would also read as `decisions` or `url`
(such as `Decisions` or `deciſions`). `start` passes `rld` only the
`https://host:port` origin of each configured router. The URL must be written canonically:
ASCII only (punycode, not Unicode, for an IDN), no `user:password@`, no
backslashes or whitespace, and a host and port spelled the way they parse
(`:443` is accepted, `:0443` or `0x7f.1` are not). Host case is ignored; a
trailing-dot host (`api.rayline.ai.`) is not the router's host.

Hybrid endpoints are selected by the core configuration. The root `--env` and
`--auth-token` options are not accepted with `hybrid`; use the configuration and
`RAYLINE_ROUTER_API_KEY` for a non-production endpoint.

## Rayline Local defaults

`start` composes the core configuration from a profile and the user's
`~/.config/rayline/settings.json`, and writes the result to `core-config.json`
in the state directory (owner-only). The user's setup is: a Rayline API key, a
`local_model` pointing at a local Qwen, and `start`.

- **Preview:** v2 is served on the development router only, until the
  production catalog migration lands. Use `start --env dev --cell
  pf-5505cbf8-v2`; a default `--env prod` start is refused by that router.
- **No profile file:** `start` builds a package-less default profile.
  - Decisions go to the signed-in router (`--env`, default `prod`) with model
    `rayline-router`.
  - `local_model` is the local Chat deployment (`registry_model`
    `qwen/qwen3.8-27b`). Its context window is `--local-context-window`, else
    settings `hybrid.local_context_window`, else 98304 tokens; a llama.cpp
    runtime loaded with a smaller window (`/props`) lowers it.
  - Claude Opus 5.5 and Sonnet 5.5 are exact model pins through the same
    router, each with its 1,000,000-token window.
  - `derived_actions` let the core derive ARC's actions from its embedded
    registry: Qwen's admitted controls, plus the default-base controls of Opus
    and Sonnet.
  - `--cell` selects an isolated development cell.
  - This profile has no prompt counter. An execution core with estimated
    capacity facts (semantic-router#357) estimates every target's prompt size,
    hands a turn that does not fit the local window to cloud before the local
    runtime sees it, and refuses a prompt no target can hold. An older core
    leaves local capacity unknown, and only a llama.cpp context refusal
    re-consults.
- **`--config`:** names a qualified profile instead.

- **Hybrid routing works with no overrides.** For the default profile, a
  `derived_actions` profile, or a profile that sets `decisions.api_mode:
  "execution_context_v2"`, the core uses that mode
  and sends `routing.policy: {"preset": "arc-policy.hybrid-default.v1"}`
  explicitly. Rayline Local never relies on an absent policy, which the
  Decisions API reads as today's package default.
- **One optional override block,** `hybrid.routing` (every key optional):

  ```json
  {"hybrid": {"routing": {"policy": "utility",
                          "schedule": {"model": ["cache_boundaries", "user_messages"]},
                          "value_usd": 0.1}}}
  ```

  `policy: "utility"` is the hybrid default preset; `"prefer_local"` selects
  `arc-policy.prefer-local.v1` (local when Stage 1 predicts it passes, else
  utility), whose threshold `min_p_pass` (in (0, 1)) may be set beside it.
  `schedule` becomes
  `routing.policy.overrides.schedule`; each dimension (`model`, `thinking`)
  lists switch-point classes from `cache_boundaries`, `user_messages` and
  `all_turns`, and the service refuses a class the package does not admit.
  `value_usd` becomes `routing.value_usd` and selects the returned `custom`
  result. Unknown keys are refused.
- **Local model.** A profile deployment marked `"from_local_model": true`
  takes its endpoint and served model from `local_model`
  (`rayline local custom --url http://127.0.0.1:11434/v1 --model qwen3.8:27b`); its protocol
  path is appended. Its runtime identity and thinking-control qualification
  still come from the profile, and a configured llama.cpp prompt counter
  verifies the served model at startup. Without a custom `local_model`, such a
  profile fails to start with the command to run.
- A profile that selects `current` or `execution_context_v1`, or names a policy
  package without setting `api_mode` (the core's default is `current`, as before
  v2), is passed through unchanged, and a `hybrid.routing` block is refused for
  it. A packaged profile must set `decisions.api_mode: "execution_context_v2"`
  to use v2.

`start` prints the applied preset and its source, the local model and any saved
key it used.

**Events come from the wire.** Harnesses run unmodified; Rayline Local adds
nothing to them. The Go core infers each request's event from what the harness
already sends (`basis: marker_inferred`): `task_start` before any assistant
message, `tool_result` versus `user_message` from the trailing turn (injected
reminders and harness-authored turns are not the user's words), the harness's
own auxiliary calls (such as Pi's compaction summary call, which sends no session
header) as stateless side calls, and a compaction by comparing the request with
the previous one, including Pi's retained tail. A harness may optionally state
events itself through the core's `X-Rayline-Event` header or `rayline_event`
member; no default depends on it.

**Typed signals (optional, no setup).** A client that wants Decisions signals
(for example `topic.operation`) adds `"routing": {"signals": ["topic.operation"]}`
to its generation request. The core passes the names to Decisions unchanged,
never forwards `routing` to the model, and returns the response's `signals`
block as compact JSON in the `x-vsr-signals` response header, as cloud VSR does.
Signals change no route. Requests without `routing` are untouched. Serving them
depends on the Decisions cell (dev cells first).

## Start and connect

Install or update Rayline using the release installer or Homebrew. The release
includes its matching execution core. Select your qualified policy and endpoint
configuration explicitly:

```bash
rayline hybrid start --config /absolute/path/to/hybrid.json
rayline hybrid status
rayline hybrid run claude -- -p "Explain this repository"
rayline hybrid run codex -- exec "Explain this repository"
rayline hybrid stop
```

For source development, build `rayline` and `rld` with
`cargo build --locked -p rayline-cli -p rayline-daemon`, set `RLD_BIN` to the built
daemon, and pass `start --core /absolute/path/to/rayline-core` to select a core
built from the matching VSR source. Startup never downloads an executable.

The default gateway is `http://127.0.0.1:20815`. Use `--port 0` on `start` for
an ephemeral port. Use `--state-dir /absolute/path` on each command for a separate
runtime instance; the default is `~/.rayline/hybrid`. A running instance must be
stopped explicitly before replacing its core or configuration.
Core initialization and health share a 120-second deadline. Set
`start --startup-timeout-secs 180` when validating a large local model artifact
needs more time; this does not trigger a runtime or model download.

`run` also renders the existing native configurations for `pi`, `opencode`,
`omp`, `hermes`, and `openclaw`. These renderers are shared with the existing
Rayline launchers. OpenClaw agent turns receive `--local` through the shared
argument adapter so they use this launch configuration; container mode is
rejected because it cannot see the hybrid gateway configuration. Claude uses
Messages; Codex uses Responses. All launches use
the virtual model `rayline-router` and the local gateway credential. Protocol
and model capabilities remain the core's responsibility.

Every harness's hybrid configuration advertises the explicit
`max_output_tokens` and `input_modalities` returned by the core's authenticated
health response, and a 1,000,000-token context window:

- Pi, omp and OpenClaw: `maxTokens`, `input`, `contextWindow`.
- OpenCode: `limit.output`, `modalities.input`, `limit.context`.
- Hermes: `context_length`. Hermes has no output setting and on Messages asks
  for its model's full ceiling, so its hybrid default provider is the
  Responses one (`rayline-resp`), where it sends no output cap.
- Claude Code: `CLAUDE_CODE_MAX_OUTPUT_TOKENS`.
- Codex: `model_max_output_tokens` and `model_context_window`.

The core computes shared limits across usable execution deployments; a source
used only for an explicit selection alias does not advertise execution
capabilities. Missing or invalid capabilities prevent startup. The core
reports the smallest window, the local model's; the harness is told 1,000,000
because a turn that outgrows the local window is handed off to cloud, and
harnesses such as Pi 1.1 cap `max_tokens` by the window that remains, so the
local window would starve that handoff. The ordinary hosted launchers keep
their existing configuration. The hybrid gateway does not reduce a submitted
output allowance: requests outside admitted limits fail visibly in the core.

For an SDK or another harness, read the gateway address from `ready.json` and
its private credential from `gateway-token` inside the state directory. Use
Bearer auth or `x-api-key`. The gateway exposes `/v1/messages`,
`/v1/chat/completions`, and `/v1/responses` as provided by the execution core.
It also requires authentication for `/healthz`.

## Session identity and restart behavior

Native session headers are preserved. An explicit `x-rayline-session-id` takes
priority; otherwise the gateway maps `x-conversation-id`, `x-parent-session-id`,
`x-session-affinity`, `x-session-id`, or `x-claude-code-session-id`, in that order.
The core extracts Claude metadata and Responses `prompt_cache_key` from the
unchanged body. Requests without usable identity must fail in the core.

Branch identity uses `x-rayline-branch-id`, then Codex `thread-id`, then
`x-claude-code-agent-id`. An OpenCode request with `x-parent-session-id` uses its
child session header for the branch. Other requests explicitly use branch
`main`. Parent and child headers reach the core unchanged. Launchers whose
harness cannot emit native identity use the existing explicit per-launch
conversation header.

Every start creates a new execution instance and new gateway/core credentials.
`status` verifies the live instance ID and both process IDs. A core crash closes
the gateway; it does not restart automatically or route to a substitute target.
After stopping or losing an instance, start a fresh harness session so its
transcript and the core's process-local execution evidence have the same lifetime.
Claude, Pi and OpenCode resume/continue/fork/session flags and Codex resume/fork commands are rejected by
this launcher. A restarted core rejects historical assistant/tool messages for an
unknown session or branch; restoring a saved session requires its durable hidden
instruction ledger, which this version does not persist. Existing harnesses also
lose access when the gateway rotates its token. The launcher cannot silently
resume them against a fresh core.
The runtime does not claim that an external server's KV cache survives or belongs
exclusively to this session.

## Process and transport contract

`rld` invokes the core as `--config PATH --listen 127.0.0.1:0`. Core auth is the
per-instance `RAYLINE_EXEC_AUTH_TOKEN`; the harness receives a different token.
The core emits exactly one startup JSON line to stdout:

```json
{"contract_version":1,"listen_url":"http://127.0.0.1:49152","pid":12345}
```

Logs go to stderr. Authenticated `/healthz` must return `ready: true`,
`contract_version: 1`, a nonempty `instance_id`, and explicit `capabilities`.
`rld` verifies the reported
PID matches the child it spawned and the URL is a loopback HTTP origin. It
publishes complete readiness metadata atomically after the health check.

`RAYLINE_EXEC_PARENT_PIPE=1` tells the core that stdin is a lifetime pipe.
`rld` holds it open, sends no input, and the core exits on EOF. This also handles
abrupt supervisor termination. Normal shutdown cancels active gateway requests,
signals only its owned core, waits up to five seconds, and then kills/reaps that
child if necessary. Inference servers remain untouched. Both cancellation before
response headers and cancellation during SSE drop the core request connection.

State directories are owner-only and credential files are mode `0600` on Unix.
The gateway applies the existing Host/Origin/JSON request guard before auth.
It is loopback-only. Direct tailnet binding, remote harness enrollment, release
packaging for the core binary, and Windows lifecycle qualification are separate
work; do not expose the credential-bearing gateway on a public interface.

## Local validation

```bash
cargo test --locked -p rayline-daemon --test it_hybrid -- --test-threads=1
cargo test --locked -p rayline-cli hybrid::tests -- --test-threads=1
cargo build --locked -p rayline-cli -p rayline-daemon
python3 scripts/test-hybrid-lifecycle.py
python3 scripts/test-hybrid-core.py \
  --core /absolute/path/to/rayline-core \
  --registry /absolute/path/to/rayline-core/thinkingcontrol/thinking_controls.compiled.json
```

For the v2 seams, `scripts/test-hybrid-v2.py --core <v2 core> --registry <registry>`
joins the real CLI, `rld`, Go core and stock installed Pi against synthetic
Decisions and inference fixtures. It checks that `start` composes the hybrid
preset, a settings schedule override and `local_model`, and that the core infers
Pi's events from its own requests: `task_start`, `tool_result` with
`last_served` and `history`, Pi's session-less summarization call as a stateless
side call, and Pi's auto-compaction (forced by a provider context overflow) as a
compaction with its retained tail at its new position. Capacity facts need a
qualified llama.cpp counter; the Go core's tests and the live acceptance cover
them.

The process tests use a local executable contract fixture to verify supervision,
auth, transparent forwarding, native fork identity, cancellation, shutdown,
restart and incompatible-core rejection. The CLI acceptance script launches the
built binaries and a harness executable that sends an actual HTTP request. These
checks do not substitute for the VSR execution-core tests or live Qwen/cloud
harness acceptance.

The joined core script launches the actual Go binary and independent local HTTP
fixtures for Decisions and model serving. It generates a synthetic policy package
using controls from the supplied public registry, then checks Messages tool use
across local/cloud/local selection, private-append replay and isolation, all three
API formats, attribution, completion-based cache age, external-service ownership,
new instance IDs on restart, and core exit when its parent's pipe closes. Its
provider responses are synthetic; it does not measure ARC accuracy or qualify a
real inference runtime.

Add `--claude` to run the installed Claude Code binary through the same stack.
It reads a synthetic temporary file using its real `Read` tool, with the first
turn on the local fixture and the result turn on the cloud fixture. The optional
`--gateway-fixture-control-url` joins the exact-dispatch app's test bootstrap;
the gateway owns fixture provider credentials and profile validation. Bootstrap
keys and profile data exist only in the isolated test environment and temporary
configuration.
Use `--pi` instead for the installed Pi harness. This fixture-backed Pi read-tool
exchange has also passed; it is separate from a live local/cloud routing proof.

For diagnosis, `--artifacts-dir /absolute/path` saves the script's synthetic
Decisions and provider-bound request bodies with owner-only permissions. These
artifacts contain no authentication headers or credential values.

To repeat the Claude exchange against an already running Qwen service, also pass
`--local-deployment /absolute/path/to/qualified-deployment.json`. This replaces the
local fixture only for the actual Claude exchange, after the baseline suite. The
deployment must include the core's verified model, build, template and prompt
counter profile. Use `--local-auth-file /absolute/path/to/private-key` to load that
runtime's credential into its declared environment variable. The script does not
start, download or stop the inference service, and checks it remains healthy
after Rayline stops. Decisions and cloud generation remain controlled fixtures;
this validates execution and accounting rather than ARC's routing quality.

## Recorded reference acceptance

On 2026-10-23, the joined script passed on macOS with the installed Claude Code
2.1.273 harness, actual Rust CLI/daemon binaries, the actual Go execution core,
and a caller-owned Qwen3.8-27B Q4_K_M server running the official llama.cpp
`b11840-36e36e63a` build. The core verified the model artifact, runtime build,
chat template and tokenizer profile before publishing readiness. The local
deployment explicitly selected the shared VSR
`inband_system: "fold_before_unit_v1"` compatibility handling: Claude's rendered
request includes a system message after a user message, which this Qwen template
cannot accept directly. The fold places later system content after the content
of the user/tool turn it governs, preserving text, cache boundaries and chronology
without moving it into the initial system preamble. This changes its wire role;
it does not preserve system-message authority. The Qwen policy action retained
its native default thinking control.

The real Qwen turn requested Claude's `Read` tool. Claude read a synthetic file,
and the following turn carried the actual tool result through the exact-dispatch
gateway with Kimi's private-append instruction. Local evidence sent to the next
decision included 2,262 provider-reported prompt tokens, 2,369 tokens in the
current rendered prefix, and 2,262 overlapping tokens, all in the same qualified
tokenizer units. Cache age was measured from completion; residency and TTL
remained unknown. These counts describe this particular synthetic prompt.

The gateway ran its real authentication, approved-profile checks, catalog pricing,
usage recording and balance debit against an isolated Firestore emulator. Its
OpenRouter provider pin was `moonshotai/mxfp4` with fallbacks disabled, matching
the fixture catalog's billed endpoint. All three cloud calls in the combined
baseline and Claude run produced exactly one usage receipt and one corresponding
balance debit each. The gateway held the synthetic provider credential; the core
used only the synthetic Rayline credential.

The same run verified private-append replay and isolation, all three API formats
in the baseline suite, authenticated shutdown, fresh credentials and instance IDs
after restart, rejection of unavailable historical execution state, a successful
fresh local request after restart, and core exit after abrupt supervisor death.
The external Qwen server remained healthy after Rayline stopped. Separate process
tests verified cancellation both before headers and during SSE, startup deadline
cleanup, and configured credential aliases reaching the core without reaching
the harness. The pinned Rust 1.88 workspace gate passed 743 tests with no failures
and two existing live tests ignored; CLI/daemon all-target clippy passed.

Decisions selection and cloud model generation in this proof were controlled
HTTP fixtures. The policy package was synthetic and used public registry
controls. This does not establish live ARC routing quality, a production cloud
deployment, measured cloud cache reuse, or qualification of other inference
servers. Claude was the only real harness in that fixture-backed local-model
run. The separate live Pi evidence below extends that coverage; other launcher
configurations and baseline wire-format checks do not imply equivalent
real-harness coverage.

Repeat the qualified local run with a separately supplied deployment and private
key file; the gateway bootstrap is available only in the isolated test setup:

```bash
python3 scripts/test-hybrid-core.py \
  --core /absolute/path/to/rayline-core \
  --registry /absolute/path/to/thinking_controls.compiled.json \
  --claude \
  --gateway-fixture-control-url http://127.0.0.1:PORT/__fixture__/configure \
  --local-deployment /absolute/path/to/qualified-deployment.json \
  --local-auth-file /absolute/path/to/private-key
```

Managed inference remains a separate ownership mode. The existing local launcher
can supply process management, but hybrid still needs a qualified launch profile
and binding of the launched runtime's port and identity before it can safely own
that process. This reference path uses a caller-owned inference runtime.
Durable resume/fork state, tailnet enrollment and Windows lifecycle qualification
also remain separate work.

## Installed execution core

Releases include `rayline-core` beside `rayline` and `rld`. `rayline hybrid start
--config <path>` discovers those companion binaries beside the resolved launcher,
including when the launcher is reached through a symlink. `--core <path>` remains
an explicit development override; `RLD_BIN` remains an explicit daemon override.
Missing installed components fail with a repair instruction, without downloading
during hybrid startup. `rayline update` repairs a missing core even when the
launcher is already at the requested release, using verified signed artifacts.
Users upgrading from an older two-binary updater may need to run it a second time.

Releases before 0.2.13 ship no core. A rollback such as
`rayline update --version 0.2.12+ee1afb00b6cd` decides this from the target's
signed `SHA256SUMS`: when it lists no `rayline-core` entry and the version is
older than 0.2.13, the update installs only `rayline` and `rld`, then removes
the installed `rayline-core` and `rayline-core-LICENSE`, so no later launcher
(for example one installed by the older two-binary updater) runs a core that
does not match it; that launcher reports the core missing and `rayline update`
installs the matching one. A 0.2.13 or later release whose `SHA256SUMS` lacks
the core fails closed. Releases are published under their full version, build
label included (shown in the GitHub release notes and by `rayline --version`);
a bare `--version 0.2.12` that is not found says to use the full form. A build newer than its
channel's latest release reports that it is up to date and names the
`rayline update --version <current>` command that repairs a missing core.

Hybrid requires macOS 13 Ventura or later, or Linux. Windows hybrid commands
fail explicitly because locking, private-state permissions and lifecycle have
not been qualified; ordinary Windows commands remain available. Historical Mac
asset names identify the existing update paths and do not express the Go core's
minimum OS. The core is built with Go 1.27.3, whose [supported Mac minimum is
macOS 13](https://go.dev/doc/go1.27#darwin).

## Live Pi acceptance

`scripts/test-hybrid-live.py` consumes a supplied real core configuration and
credential environment. It starts only `rld` and the Go core. It creates no
Decisions or inference fixtures and does not start the model runtime. The two
tasks run in fresh Pi sessions with isolated synthetic working directories.
An oracle in the already-running parent process verifies a ledger answer and a
resource-constrained dependency scheduler against cases kept outside the agent's
editable task directory. Agent-written tests do not determine success.

The required `--proof routing` mode verifies that the same configured custom
`value_usd` and caller price identity were applied to both tasks. Supply inline
`prices` or a service `price_profile_id`; acceptance requires an explicit
`price_profile_version` (a positive integer), configured `price_sha256`, and
`--price-profile-content-hash sha256:<hex>` for a profile. These are separate
hashes: `price_sha256` is the bare 64-character RFC8785 digest of the routing
prices; the profile content hash identifies the saved fixed-order price table.
Acceptance verifies both hashes plus the echoed profile ID and version on every
decision. Ordinary requests may use a bare profile ID to resolve the latest version each turn. All successful
inference for the simple task must execute locally; the scheduler's final
generation must execute in the cloud. The report links each actual ARC selection
to completed execution and records selected and executed model identities
separately, including explicit aliases. Unexpected route choices fail the proof.

The distinct `--proof transport` mode requires real `top` with empty routing and
reports actual route choices without claiming low-V or discounted-price support.
It is useful while the live Decisions service does not support those controls;
it does not satisfy the routing acceptance criteria. Neither mode rewrites the
supplied configuration or quietly drops requested controls.

Live runs require a positive process-wide `max_generation_attempts` in the core
config. Failed and partial inference attempts consume that cap. The runner also
enforces a per-task generation monitor and wall timeout, terminating only its
owned Pi process group on failure. A structured `decision refused` event is
preserved as evidence and stops the task immediately instead of waiting for
harness retries against the unchanged configuration. Supply an operator-verified
`--generation-cost-bound-usd`; the total bound across configured inference
attempts must fit `--max-run-cost-usd` (default $5). This is an inference cost
bound, not a billing measurement; any separately charged Decisions calls must
also fit the operator's budget. Evidence is saved in a new private output
directory even when a task or route assertion fails.

```bash
python3 scripts/test-hybrid-live.py \
  --core /absolute/path/to/rayline-core \
  --config /absolute/path/to/live-hybrid.json \
  --output-dir /absolute/path/to/new-proof-directory \
  --proof routing \
  --generation-cost-bound-usd 0.25
```

On 2026-10-23, installed Pi 0.80.2 completed both tasks through the real
Decisions service and real inference using the same configuration. The ledger
used three generations: cloud Haiku → on-device Qwen 27B → cloud Haiku, including
file read and write tools. The parent oracle verified quantity 10 and total
1,284 cents. The local turn used an explicit test alias from ARC's selected
DeepSeek Flash action to the qualified Qwen deployment; selection and execution
were recorded separately. This establishes mixed local/cloud execution, not a
calibrated ARC decision for Qwen.

The scheduler completed in six cloud generations: Haiku → Sol → Haiku → Haiku →
Opus → Haiku. Its implementation passed all 19 independent oracle cases and the
input-mutation check. Both tasks had matching decision and completed-execution
records, and the runner cleaned up its owned core and gateway successfully.

That run used `top` with empty routing. It does not establish the requested
low-V, discounted-Qwen-price acceptance or an entirely local simple task. The
limits were 4,096 output tokens per request, six generations per task, twelve
process-wide inference attempts, and 300 seconds per task, with a $4.80
operator-verified inference cost bound. The current Rust workspace gate passed
745 tests with no failures and two existing ignored tests; workspace all-target
clippy and formatting passed.
