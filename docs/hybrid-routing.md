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

For hosted Decisions and Rayline cloud inference, provide
`RAYLINE_ROUTER_API_KEY` explicitly in the launch environment. Hybrid does not
load saved production credentials for destinations named by a separate config.
The core rejects missing credential environment references before startup. The
launcher withholds every configured credential environment variable from the
harness, including custom Decisions and inference key names. Readiness metadata
contains only those names; the harness receives its separate gateway token.
A local Decisions
provider can be selected through the core configuration when its contract is
supported by the core; Rust performs no policy selection.

Hybrid endpoints are selected by the core configuration. The root `--env` and
`--auth-token` options are not accepted with `hybrid`; use the configuration and
`RAYLINE_ROUTER_API_KEY` for a non-production endpoint.

## Start and connect

Build `rayline` and `rld` from this checkout, and build `rayline-core` from the
matching VSR checkout. The core and policy configuration are explicitly selected;
this command does not download an unversioned core executable.

```bash
cargo build --locked -p rayline-cli -p rayline-daemon
export RLD_BIN="$PWD/target/debug/rld"

target/debug/rayline hybrid start \
  --core /absolute/path/to/rayline-core \
  --config /absolute/path/to/hybrid.json

target/debug/rayline hybrid status
target/debug/rayline hybrid run claude -- -p "Explain this repository"
target/debug/rayline hybrid run codex -- exec "Explain this repository"
target/debug/rayline hybrid stop
```

The default gateway is `http://127.0.0.1:20815`. Use `--port 0` on `start` for
an ephemeral port. Use `--state-dir /absolute/path` on each command for a separate
runtime instance; the default is `~/.rayline/hybrid`. A running instance must be
stopped explicitly before replacing its core or configuration.
Core initialization and health share a 120-second deadline. Set
`start --startup-timeout-secs 180` when validating a large local model artifact
needs more time; this does not trigger a runtime or model download.

`run` also renders the existing native configurations for `pi`, `opencode`,
`omp`, `hermes`, and `openclaw`. These renderers are shared with the existing
Rayline launchers. Claude uses Messages; Codex uses Responses. All launches use
the virtual model `rayline-router` and the local gateway credential. Protocol
and model capabilities remain the core's responsibility.

Pi's hybrid model advertises the explicit `context_window`, `max_output_tokens`
and `input_modalities` returned by the core's authenticated health response. The
core computes shared limits across usable execution deployments; a source used
only for an explicit selection alias does not advertise execution capabilities.
Missing or invalid capabilities prevent startup. Pi's ordinary hosted launcher
keeps its existing configuration. The hybrid gateway does not reduce a submitted
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
Claude and Pi resume/continue/fork flags and Codex resume/fork commands are rejected by
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
Durable resume/fork state, tailnet enrollment, core release packaging and Windows
lifecycle qualification also remain separate work.

## Live Pi acceptance

`scripts/test-hybrid-live.py` consumes a supplied real core configuration and
credential environment. It starts only `rld` and the Go core. It creates no
Decisions or inference fixtures and does not start the model runtime. The two
tasks run in fresh Pi sessions with isolated synthetic working directories.
An oracle in the already-running parent process verifies a ledger answer and a
resource-constrained dependency scheduler against cases kept outside the agent's
editable task directory. Agent-written tests do not determine success.

The required `--proof routing` mode verifies that the same configured custom
`value_usd` and caller price identity were applied to both tasks. All successful
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
owned Pi process group on failure. Supply an operator-verified
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

On 2026-10-23, installed Pi 0.80.2 completed the ledger task through the real
Decisions service and real inference: cloud Haiku → on-device Qwen 27B → cloud
Haiku, including file read and write tools. The parent oracle verified the
result. The local turn used an explicit test alias from ARC's selected DeepSeek
Flash action to the qualified Qwen deployment; selection and execution were
recorded separately. This establishes mixed local/cloud execution, not a
calibrated ARC decision for Qwen. The scheduler task stopped visibly when ARC
selected an additional model without an execution binding; it did not pass.

That run used `top` with empty routing. It does not establish the requested
low-V, discounted-Qwen-price acceptance, an entirely local simple task, or a
completed cloud scheduler task. The process cap was eight inference attempts
with 2,048 output tokens per request and a $4 operator-verified inference cost
bound. The current Rust workspace gate passed 745 tests with no failures and
two existing ignored tests; workspace all-target clippy and formatting passed.
