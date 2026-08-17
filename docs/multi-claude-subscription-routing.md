# Multi-Subscription Claude Routing

Status: **Implemented locally — synthetic and basic live acceptance complete**

Last updated: 2026-07-31

## Summary

Rayline makes several Claude subscriptions look like one local subscription
pool without launching Claude Code under several aliases or configuration
directories.

Claude Code always runs with one **control profile**, such as `~/.claude`.
That profile remains the source of settings, sessions, projects, MCP
configuration, account UI, and daemon state. Existing directories such as
`~/.claude-memex` and `~/.claude-ws` are registered with Rayline only as
**credential sources**. Rayline selects an eligible credential locally for
each Anthropic Messages request.

The proxy monitors every account's global and model-scoped allowance. It
preserves affinity to one account while it remains usable, then retries a
request through another account only when Anthropic explicitly rejects the
first account before streaming a response. For example, exhausting the Fable
allowance on one subscription would move Fable requests to another
subscription without unnecessarily moving Sonnet requests.

This document records the intended behavior and the evidence gathered before
implementation began. It deliberately separates observed Claude Code behavior
from private interfaces that still require compatibility guards.

## Implementation Progress

The local implementation now spans the CLI, daemon, transparent proxy, and the
new shared `crates/rayline-subscriptions` crate. It includes:

- the versioned pool configuration model and validation
- `subscriptions add`, `remove`, `list`, `status`, and `reload`
- one shared control `CLAUDE_CONFIG_DIR` with credential-only source profiles
- macOS Keychain and profile-local credential-file backends
- serialized OAuth refresh, refresh-token rotation, compare-and-swap writes,
  private file permissions, and invalid-grant quarantine
- a forward-compatible `/api/oauth/usage` response schema
- startup and background per-account usage polling
- normalized global and model-scoped claims
- Fable, Sonnet, Opus, and Haiku model-family normalization
- minimum-applicable-headroom selection with active-lease-aware new-launch
  balancing, a launch primary, and model-family overrides
- private launch-scoped assignment snapshots and `rld statusline` subscription
  and JSON components
- `included_only` overage handling
- a dedicated pool proxy state directory and default proxy/metrics ports, so a
  pooled launch cannot silently reconfigure an ordinary Rayline proxy
- unified response-header normalization and request-result classification
- local proxy credential replacement only on Anthropic `POST /v1/messages`
- bounded pre-stream failover, single-flight 401 refresh, and no replay after an
  ambiguous network failure or downstream streaming
- removal of per-account unified-limit headers from pooled responses
- sanitized usage and weekly-limit fixtures
- local fake-service integration tests for Fable-only failover, model isolation,
  concurrent refresh-token rotation, and invalid-grant quarantine

The remaining release work is a deliberately induced live hard-limit failover,
broader Claude Code version compatibility verification, and product/legal
review of subscription pooling.

## Quick Start

Register existing profiles. The first command chooses the shared Claude Code
control directory; later profile directories are credential sources only:

```bash
rayline subscriptions add af \
  --claude-config-dir ~/.claude \
  --control-config-dir ~/.claude
rayline subscriptions add memex \
  --claude-config-dir ~/.claude-memex
rayline subscriptions add ws \
  --claude-config-dir ~/.claude-ws

rayline subscriptions list
rayline subscriptions status
```

The default status view is a compact routing summary: it shows remaining
five-hour, weekly, and Fable capacity, the Fable reset, which model families are
available, the most relevant limiting reset, and active launch counts when a
pool daemon is running.
It also projects each bucket's run-out from the current window's average burn.
A timestamp is shown only when depletion is projected before reset;
`reset first` means renewal is expected to win, and `learning` suppresses a
noisy estimate near the start of a window. Forecasts are planning hints rather
than guarantees: a workload change immediately changes the real burn rate.
Use `rayline subscriptions status --verbose` for every normalized provider
claim and placement counter, or `--json` for the complete structured payload.
Background consumers should add `--live-only`; when no pool daemon is running,
it fails instead of opening credential stores to build a standalone snapshot.

Then start Claude Code:

```bash
rayline claude --subscription-pool default
```

Selecting a pool implies proxy mode with a subscription-backed main thread
(`--route subagents`) unless a router config derives the same passthrough-main
shape. `--subscription-pool` is rejected with `--via env`, `--isolated`, and an
explicit `--route all`. Use `--subscription-config <path>` to override
`~/.config/rayline/subscriptions.json`.

The registry contains paths and policy only. It is written mode `0600`; OAuth
tokens remain in the original Claude credential backends.

On macOS, the first signed `rld` that opens each Claude Keychain item may trigger
one Keychain approval per registered account. Choose **Always Allow** for the
stable `ai.rayline.rld` identity. A running daemon keeps credentials in memory,
but token rotation must reread and update the source safely; the stable
Developer ID requirement lets those later accesses proceed without another
popup. If Anthropic rejects a cached refresh token because another Claude
process rotated that profile's credential, `rld` reopens only that source once
and adopts the newer version. An unchanged rejected credential is quarantined
and its refresh token is never sent again. A quarantined account rereads only
its own credential source, and only on the usage-poll tick, so signing in to
that profile again restores it without a daemon restart.

Production installers and self-updates reject identity-unstable macOS `rld`
binaries. For local development, sign with a persistent Developer ID or Apple
Development identity using `scripts/sign-macos-rld.sh`; ad-hoc signing ties
approval to one exact build hash and causes prompts again after rebuilding.

`rayline subscriptions status` first asks the running subscription-pool daemon
for its already-loaded allowance snapshot and live placement counters. That
path does not reopen Keychain. If no pool daemon is reachable, the command
falls back to a standalone allowance poll, labels live placement unavailable,
and may need Keychain access. Pool launches use port `20816` by default; set
`RAYLINE_SUBSCRIPTION_METRICS_PORT` consistently for a custom port.

`rayline subscriptions reload` is the one command that deliberately makes the
daemon reread credentials. It asks the running daemon to reopen every account's
credential source, so a profile you signed in to again is adopted at once
instead of on the next usage-poll tick. The daemon does the reading, so the
account may need Keychain access; the CLI only prints what changed:

```text
Subscription pool: default
  af  Quarantined → Healthy  reloaded a new credential from the credential source
  ws  Healthy → Healthy  unchanged
```

With no daemon running there is nothing to correct, so the command reports that
the next launch reads the credential sources anyway and exits successfully.

Only that one case is a success. A daemon that is running but did not reload
still holds the credential the user asked it to replace, so the command says
what happened and exits non-zero: the reload timed out, the daemon answered with
an error status, its answer could not be read, or it serves another pool. The
endpoint takes no pool selector, so a daemon serving another pool reloads that
pool; the message names the pool that was reloaded.

The daemon serves the reload as `POST /v1/subscriptions/reload` on the same
loopback-only control port as the status snapshot, and it requires
`content-type: application/json`. A request without that content type is
rejected with `415` before any credential source is opened. The status route is
an in-memory read, but a reload reopens every account's credential store, so the
route needs one guard the read does not: a web page the user visits over `http`
can POST to a fixed loopback port, and a cross-origin request may only carry a
form or text content type unless a CORS preflight succeeds. This server answers
no preflight and returns no CORS headers. The check is not authentication —
every local process may still call the route, which is the trust level the
loopback bind already grants.

### Show the serving subscription in Claude's status line

Pool launches export a launch-scoped status identifier. The proxy uses it to
write the current serving account, model-family override, failover reason, and
remaining bottleneck headroom plus model-aware eligible/total pool counts
without storing credentials. Compose that data
into an existing `~/.claude/bin/statusline` script with:

```bash
input=$(cat)
subscription=$(
  printf '%s' "$input" |
    rld statusline --component subscription 2>/dev/null
)
[ -z "$subscription" ] || printf '%s' "$subscription"
```

The compact fragment is shaped like `◈ af · 5h 99%L · 3/3`, or
`◈ af→ws · F 70%L · 1/3` after a Fable-specific failover. `L` means allowance
left and the final fraction is eligible subscriptions over configured
subscriptions for the current model. For pooled launches this fragment should
replace a `CLAUDE_CONFIG_DIR`-derived account badge and any per-profile usage
poll; retain those only as a non-pooled fallback.

For custom formatting, use
`rld statusline --component subscription --json`. The status-line reader only
reads a bounded local sidecar; it does not open Keychain, load OAuth
credentials, or contact Anthropic. See
[Session-Aware Claude Subscription Placement and Status](claude-session-aware-subscription-placement.md#rld-statusline-contract)
for the JSON schema and composition examples.

The default `rld statusline` output composes that subscription fragment with
the latest Rayline-selected model for the same launch. Pooled launches never
read or overwrite the legacy global route sidecar, so concurrent Claude
processes cannot display one another's router decision.

## Live Acceptance

On 2026-07-29, Claude Code 2.1.220 was exercised interactively through the
three-account test pool using one control `CLAUDE_CONFIG_DIR`.

- The startup poll returned fresh global and Fable-scoped allowance for all
  three credential sources.
- A normal Opus request traversed
  `POST /v1/messages?beta=true` and returned the expected response.
- An explicit Fable request resolved to `claude-fable-5`, traversed the same
  pooled Messages path, and returned the expected response.
- Control-plane requests such as bootstrap, account settings, MCP discovery,
  and event logging remained control-profile passthrough traffic.
- A second launch reused the same pool daemon PID. This confirms that the
  daemon retains credentials in memory instead of reopening all Keychain items
  per request or launch.
- The pool proxy used its dedicated state directory and ports `20815`/`20816`;
  no ordinary shared proxy was replaced.

The live run did not intentionally consume an account to force a provider
rejection. Exact unified-429 failover, final-rejection preservation,
model-family isolation, refresh serialization, and invalid-grant quarantine
remain covered by local fake-provider integration tests.

## Decisions

The first implementation should follow these constraints:

1. Claude Code uses one control `CLAUDE_CONFIG_DIR` for every launch.
2. Additional profile directories supply OAuth credentials only. Claude Code is
   never launched with them.
3. Subscription selection and token use stay on the user's machine. Hosted
   Rayline services never receive Claude subscription credentials.
4. Pooling applies initially only to requests that Rayline has classified as
   Anthropic subscription traffic, and only through `--via proxy`.
5. Only `POST /v1/messages`, including query variants such as
   `/v1/messages?beta=true`, is account-switched in v1. Bootstrap, account,
   usage, MCP, login, and other control-plane requests continue using the
   control profile.
6. Selection considers every limit applicable to the requested model. It does
   not treat a single high-headroom bucket as proof that an account is usable.
7. Account affinity is sticky by launch and model family. The pool is for
   failover and allowance-aware placement, not request-by-request round robin.
8. A request is replayed on another account only after an explicit,
   pre-response account quota or entitlement rejection. It is never replayed
   after response bytes have reached Claude Code.
9. The default billing policy is `included_only`: do not consume paid extra
   usage merely because an account has it enabled.
10. Existing `subscription` route semantics remain intact. A pool augments that
    local subscription target; it does not replace local inference or hosted
    routing.

## Goals

- Remove the need for aliases such as `claude-af`, `claude-ws`, and
  `claude-memex` during normal use.
- Share one Claude Code configuration, daemon, session history, settings, MCP
  configuration, and UI state.
- Use several subscriptions that the user has explicitly registered.
- Route based on five-hour, weekly, and model-scoped allowance.
- Fail over a rejected request before Claude Code sees the rejection.
- Keep separate model families on different accounts when their scoped limits
  differ.
- Preserve prompt-cache locality and avoid needless account switching.
- Provide one local status view showing the effective pool and why accounts are
  eligible, deprioritized, or unavailable.
- Keep credentials, prompts, and routing decisions out of normal logs.

## Non-goals

- Creating, purchasing, or discovering Anthropic accounts.
- Automatically combining personal and employer-managed accounts.
- Bypassing plan, organization, billing, or provider policy.
- Fabricating model entitlements that the control profile does not expose.
- Balancing every request evenly across accounts.
- Retrying ambiguous network failures that might have reached the model.
- Making Claude Code's built-in `/usage` screen represent the whole pool in v1.
- Depending permanently on undocumented Claude Code internals without version
  detection and a safe fallback.

## Baseline Rayline Behavior

Before pool selection, Rayline supports a single Claude subscription identity
per Claude Code process:

- `rayline-cli` resolves `CLAUDE_CONFIG_DIR`, defaulting to `~/.claude`.
- Proxy mode removes Anthropic API-key environment variables so Claude Code can
  use its own OAuth session.
- For direct Anthropic subscription traffic without a configured pool,
  `rayline-proxy` preserves the incoming Claude OAuth authorization.
- For hosted router traffic, the proxy removes the incoming authorization and
  injects a Rayline router key.
- The `subscription` endpoint in router configuration means “send using the
  caller's existing subscription authorization”; it is a sentinel, not a
  separately configured endpoint.
- On Unix, the launcher ultimately replaces itself with Claude Code. It cannot
  change `CLAUDE_CONFIG_DIR` and relaunch transparently halfway through a
  session.
- The proxy currently sends one outbound request and immediately begins
  adapting the upstream response.

The multi-alias setup documented in `~/code/local-admin` works by assigning a
different `CLAUDE_CONFIG_DIR` to each alias. Durable content can be shared
between those directories, but authentication, settings, daemon state, and
some runtime state remain profile-local. This proves that the subscriptions can
coexist, but exposes account selection to the user and fragments Claude Code's
runtime state.

## Observed Limit Interfaces

The observations in this section were made on 2026-07-29 with Claude Code
2.1.220. They are implementation evidence, not a stable public Anthropic
contract.

### Usage endpoint

Claude Code calls:

```text
GET https://api.anthropic.com/api/oauth/usage
anthropic-beta: oauth-2025-04-20
```

The observed response contains these top-level limit buckets:

- `five_hour`
- `seven_day`
- `seven_day_oauth_apps`
- `seven_day_opus`
- `seven_day_sonnet`
- `cinder_cove`
- `extra_usage`
- `limits`

The main buckets have `utilization` and `resets_at`. Endpoint utilization is a
percentage in the range `0..100`.

`limits` is an extensible collection. An observed item can contain:

```json
{
  "kind": "weekly_scoped",
  "group": "...",
  "percent": 47,
  "resets_at": "2026-08-02T02:00:00Z",
  "scope": {
    "model": {
      "id": null,
      "display_name": "Fable"
    }
  },
  "is_active": true,
  "severity": "..."
}
```

Fable is identified by `scope.model.display_name`; its observed `id` is null.
The selector must therefore normalize model IDs and display names rather than
depending on a stable scoped-limit ID.

`is_active` must not be interpreted as “this bucket is available.” Live results
show it behaving more like a current binding or highlighted constraint. Every
applicable bucket must be evaluated whether or not `is_active` is true.

Claude Code appears to persist usage for approximately five minutes and retains
a saved result for up to approximately one hour. Rayline can use comparable
timings, but should own its cache rather than depend on Claude's.

### Unified response headers

Anthropic Messages responses can include a richer set of
`anthropic-ratelimit-unified-*` headers. Observed or recognized fields include:

- overall `status`, `reset`, `fallback`, and `representative-claim`
- overage `status`, `reset`, `disabled-reason`, and `in-use`
- upgrade paths and overage-period utilization
- per-claim utilization, reset, status, and surpassed threshold

Observed abbreviated claims map as follows:

| Header claim | Internal meaning |
| --- | --- |
| `5h` | five-hour/session allowance |
| `7d` | global weekly allowance |
| `7d_oi` | weekly overage-included/model allowance, currently used for Fable |
| `overage` | paid usage credits |

Header utilization is expressed in the range `0..1`, unlike the usage
endpoint's `0..100`. Rayline must normalize both sources into one internal
representation.

### Sanitized hard-limit response

A real Claude Code request against an exhausted weekly subscription returned
the following shape before generating tokens:

```http
HTTP/1.1 429 Too Many Requests
content-type: application/json
x-should-retry: true
anthropic-ratelimit-unified-status: rejected
anthropic-ratelimit-unified-5h-status: allowed
anthropic-ratelimit-unified-5h-utilization: 0.0
anthropic-ratelimit-unified-7d-status: rejected
anthropic-ratelimit-unified-7d-utilization: 1.0
anthropic-ratelimit-unified-7d-surpassed-threshold: 1.0
anthropic-ratelimit-unified-representative-claim: seven_day
anthropic-ratelimit-unified-reset: <epoch>
retry-after: <seconds>
anthropic-ratelimit-unified-overage-status: rejected
anthropic-ratelimit-unified-overage-disabled-reason: out_of_credits
```

```json
{
  "type": "error",
  "error": {
    "type": "rate_limit_error",
    "message": "This request would exceed your account's rate limit. Please try again later."
  },
  "request_id": "<redacted>"
}
```

Claude Code reported `api_error_status: 429`, zero input tokens, zero output
tokens, and zero cost. Although `x-should-retry` was true, Claude Code recognized
the unified quota headers and did not treat the response as a normal transient
429.

This is the main reactive failover signal. The request body is still buffered
by Rayline at this point and no response has been forwarded downstream, so
retrying it with another eligible subscription is safe.

A handcrafted request without Claude Code's complete request shape instead
received a generic 429 without unified headers. Tests must use sanitized
captures from real Claude requests; an arbitrary API request is not sufficient
to characterize subscription exhaustion.

### Point-in-time pool example

The three sanitized local profiles had these readings during the investigation:

| Credential source | Five-hour | Weekly | Fable scoped | Effective state |
| --- | ---: | ---: | ---: | --- |
| `af` | 0% | 100% | 100% | global weekly exhausted |
| `memex` | 100% | 21% | 38% | five-hour exhausted |
| `ws` | 8% | 32% | 47% | eligible |

At that moment, both Sonnet and Fable should select `ws`. After the five-hour
reset, `memex` should re-enter the candidate set. If `ws` later exhausts only
its Fable-scoped allowance, Fable should move while Sonnet can remain sticky on
`ws`.

These readings illustrate the algorithm; they are not fixtures or normative
defaults.

## Proposed Architecture

```text
                         one CLAUDE_CONFIG_DIR
                                  |
                                  v
                         +-------------------+
                         |    Claude Code    |
                         | control profile   |
                         +---------+---------+
                                   |
                          local HTTPS proxy
                                   |
                   +---------------+----------------+
                   | request classification/target  |
                   +--------+-------------+----------+
                            |             |
                 router/local target      | subscription target
                            |             v
                            |    +--------------------+
                            |    | subscription pool  |
                            |    | selector + state   |
                            |    +---+------------+---+
                            |        |            |
                            |     worker af     worker ws
                            |        |            |
                            +--------+------------+
                                     |
                                  Anthropic
```

### One control profile

Every launch uses the same Claude configuration directory:

```bash
CLAUDE_CONFIG_DIR="$HOME/.claude" rayline claude --via proxy
```

The control profile owns:

- settings and feature flags
- projects, sessions, plans, tasks, and history
- MCP and plugin configuration
- Claude Code daemon state
- account/bootstrap UI and built-in `/usage`
- the inbound OAuth identity for endpoints that are not pooled

Rayline must not swap `CLAUDE_CONFIG_DIR`, copy complete profiles, or start one
Claude daemon per subscription.

### Credential sources

Each additional existing profile directory is registered as a credential
source. Rayline reads only the credentials associated with that source; it does
not load its settings, history, projects, or daemon state.

The sources are logically credential stores, even if the first implementation
locates them by their historical Claude config directory. A future migration
could move them to a Rayline-owned credential store without changing selection
semantics.

Credential sources cannot be strictly read-only. OAuth access tokens expire,
and refresh tokens may rotate. A per-source worker must atomically persist a
refreshed credential back to the same source. “Credential-only” therefore
means that Rayline ignores all non-authentication state, not that it never
writes the credential store.

### Request classification

Pool selection happens after Rayline has determined the request's routing
target:

- Requests selected for a hosted or local router continue to that router.
- Requests selected for the existing `subscription` target enter the
  subscription pool.
- In `--route subagents` mode, direct main-agent subscription traffic can use
  the pool while selected subagents continue to their configured router.

Within the subscription target, v1 switches credentials only for
`POST /v1/messages`. These requests use the chosen worker's OAuth token.

The following stay on the control profile credential:

- `/api/oauth/usage`
- `/api/oauth/account/settings`
- bootstrap and feature discovery
- MCP registry and `/v1/mcp_servers`
- login, logout, and control-profile refresh
- unrecognized endpoints

Keeping control-plane traffic stable prevents account UI, model catalogs, and
MCP state from oscillating between subscriptions. It also limits the surface
area that relies on private behavior.

Count-token and other model endpoints can be added later if a real request
trace proves that account affinity is required for them.

### Local egress workers

Each account has a local egress worker that owns:

- credential loading
- access-token expiry tracking
- refresh serialization
- atomic credential persistence
- usage polling
- normalized limit state
- temporary health and quarantine state

For a selected Messages request, the proxy removes the inbound
`Authorization` and `x-api-key` values and lets the local worker attach its
current access token. Raw tokens never appear in pool configuration, process
arguments, metrics, or ordinary logs.

The worker should refresh asynchronously when expiry is between roughly 30 and
120 seconds away. If a request sees a token within the immediate refresh
window, it waits for the one shared refresh operation. A rotated refresh token
is written back using compare-and-swap semantics where the credential backend
allows it. An `invalid_grant` first triggers a versioned reload of that source:
if standalone Claude has written a newer token pair, the worker adopts it and
remains healthy; only an unchanged rejected token is quarantined.

A quarantine is not permanent. The rejected refresh token is dead, so the
worker never sends it again, but each usage poll rereads that one credential
source. A new document there — which is what signing in to the profile again
writes — is adopted, and the account returns to selection.

### Launch affinity

Several Claude Code launches can share one proxy and still need independent
stickiness. The proxy therefore needs an opaque launch ID.

The preferred approach is a random per-launch capability conveyed using local
proxy basic authentication, which Claude Code supports for corporate proxy
configuration. The credential is for local routing identity, not Anthropic
authentication, and must never be logged.

If that transport cannot be made reliable across Claude Code versions, the
fallback is a separate loopback listener per launch. The rest of the pool
design should not depend on which mechanism supplies the launch ID.

The initial affinity key is:

```text
(launch_id, model_family)
```

An agent or conversation identifier may be added when Rayline can obtain one
reliably. It is not required for v1.

## Pool Configuration and CLI

The registry at `~/.config/rayline/subscriptions.json` uses this shape:

```json
{
  "schema": 1,
  "pools": {
    "default": {
      "control_config_dir": "~/.claude",
      "accounts": [
        {
          "id": "af",
          "credential_source": {
            "claude_config_dir": "~/.claude"
          }
        },
        {
          "id": "memex",
          "credential_source": {
            "claude_config_dir": "~/.claude-memex"
          }
        },
        {
          "id": "ws",
          "credential_source": {
            "claude_config_dir": "~/.claude-ws"
          }
        }
      ],
      "policy": {
        "billing": "included_only",
        "switch_at_percent": 90,
        "sticky": "launch_model_family"
      }
    }
  }
}
```

The configuration stores paths, stable local IDs, and policy only. It must
never contain tokens or copied credential payloads.

The CLI exposes:

```bash
rayline subscriptions add af --claude-config-dir ~/.claude \
  --control-config-dir ~/.claude
rayline subscriptions add memex --claude-config-dir ~/.claude-memex
rayline subscriptions add ws --claude-config-dir ~/.claude-ws
rayline subscriptions status
rayline subscriptions reload

rayline claude \
  --subscription-pool default
```

`--subscription-pool` is rejected with `--via env`, because env routing
does not provide the request-level interception required for selection and
failover. It also rejects `--isolated` (which contradicts the shared-control
directory contract) and explicit `--route all` (which has no Anthropic
subscription Messages leg to pool).

Registration must be explicit. Rayline should not scan the home directory and
silently pool every Claude profile it finds.

## Limit Model

Rayline should normalize all observed sources into claims:

```text
LimitClaim {
    key
    scope              // global, model family, surface, or unknown
    utilization        // normalized 0.0 .. 1.0
    status             // allowed, rejected, unknown
    resets_at
    source             // usage endpoint or response header
    observed_at
}
```

Unknown fields and unknown claim names should be preserved. The parser should
be tolerant of additions and strict only about the fields required to make a
decision.

### Applicable limits

For a request, the applicable set is:

1. the global five-hour claim
2. the global weekly claim
3. every scoped claim matching the normalized requested model family
4. relevant entitlement or usage-credit state
5. any subsequently discovered global claim

For example, a Fable request is constrained by the five-hour, global weekly,
and Fable weekly-scoped claims. A low Fable utilization does not make an
account eligible when its global weekly allowance is exhausted.

Effective headroom is:

```text
minimum(1.0 - utilization) across all applicable included-usage claims
```

The usage endpoint's current Fable representation should map known request
names such as `claude-fable-5` and Fable aliases to the normalized `Fable`
family. Matching should be case-insensitive and version-aware.

### Selection policy

The selector is intentionally conservative. The follow-up
[Session-Aware Claude Subscription Placement and Status](claude-session-aware-subscription-placement.md)
design is now implemented: new launches use active-lease-aware balancing,
retain a primary account, and expose model overrides and router decisions
through a launch-scoped status snapshot.

Selection proceeds in this order:

1. Exclude accounts with unhealthy or unavailable credentials.
2. Exclude accounts known not to have the requested entitlement.
3. Exclude accounts with any applicable hard-rejected or fully exhausted
   included-usage claim.
4. Preserve the affinity account if it remains eligible and is below the soft
   switch threshold.
5. Otherwise prefer eligible accounts below the soft threshold, ordered by
   effective headroom divided by the applicable active lease count.
6. If every eligible account is above the soft threshold but still has
   included allowance, choose the account with the most effective headroom.
   A soft threshold must not strand usable capacity.
7. Use a launch-specific stable tie-breaker to distribute equal-capacity new
   launches without oscillating an existing assignment.

The selector should not round robin. Reusing an account for the same launch and
model family improves prompt-cache locality and makes status easier to reason
about.

Concurrent in-flight requests may race against the last available allowance.
That is acceptable: an authoritative hard 429 reconciles state and retries the
rejected request on a different eligible account.

### Billing policy

With `billing: included_only`, extra usage is not considered available pool
capacity. If paid overage is enabled for an account, Anthropic may allow a
request instead of returning an included-limit 429. Proactive polling and the
soft threshold are therefore necessary to move before paid usage begins.

An `overage-in-use` response signal immediately marks the account as unsuitable
under `included_only`. A future explicit `allow_overage` policy can model cost
and caps separately; it should not be implicit.

## Monitoring and Reconciliation

The pool should combine proactive snapshots with authoritative per-request
feedback:

- Fetch `/api/oauth/usage` for each account at daemon startup.
- Refresh healthy accounts approximately every five minutes.
- Refresh every 30–60 seconds when an applicable claim approaches the soft
  threshold or reset.
- Update state from unified headers on every Messages response.
- Treat an explicit unified rejection as authoritative immediately.
- At a reported reset time, mark the claim unknown and refetch rather than
  assuming the allowance has reset exactly on schedule.
- Preserve the last known state through short polling failures, but mark its
  freshness.

An account with unknown usage is not the first choice when a known-healthy
account exists. It remains probeable when no known candidate can serve the
request; an explicit response then establishes its state. A usage-endpoint
failure alone must not permanently quarantine an account.

The usage endpoint is private and may change. Reactive response handling is the
final authority for hard exhaustion; polling is an optimization and a
paid-overage guard.

## Retry and Failover Rules

The proxy already buffers the request body before sending upstream. The new
send path should retain that body until it decides whether the response can be
forwarded.

| Upstream result | Pool action |
| --- | --- |
| `429` with unified `rejected` plus a representative, overage, or disabled claim | Mark the exact account claim exhausted and try the next eligible account before forwarding |
| Generic `429` without unified quota evidence | Do not update allowance state or rotate accounts; pass it through for Claude Code's normal provider backoff |
| `529` or overloaded response | Do not rotate accounts; this is provider-wide, not account-specific |
| First `401` | Refresh the selected worker once and retry the same account |
| Refresh `invalid_grant` after another process changed the credential | Reload that source once, adopt the newer token, and retry the same account |
| Repeated `401` or `invalid_grant` with an unchanged credential | Quarantine that credential source and select another account if safe |
| A quarantined source later holds a new credential document | Adopt it on the next usage poll, or at once on `rayline subscriptions reload`, and return that account to selection |
| Explicit pre-stream entitlement or `credits_required` rejection | Mark the relevant model/account unavailable under policy and select another eligible account |
| Network disconnect or timeout after send | Do not replay on another account because processing is ambiguous |
| `200` or any response body/SSE bytes forwarded | Never replay |

Response headers update allowance state only after the response has been
classified. Successful responses remain useful live observations, and a `429`
updates hard-exhaustion state only when the complete unified rejection evidence
above is present. Partial claim headers on transient or provider-capacity errors
must not override the usage snapshot.

The observed `credits_required` detail and
`seven_day_overage_included`/Fable mapping need a sanitized live fixture before
they are treated as a precise production classifier. Until then, unknown scoped
claims should be associated conservatively with the requested model rather than
marking the entire account exhausted.

When attempted subscriptions reject the request, Rayline returns the final
real unified 429 response intact. Claude Code can then stop retrying and show
its standard reset message. If no account can serve the request before any
upstream attempt, Rayline returns a bounded local 429 instead; there is no
current provider response to preserve in that case. That local 429 names every
account and why it was passed over, because accounts are blocked for different
reasons and only one of them is spent allowance. The message is one line,
wrapped here to fit:

```text
Claude subscription pool "default" has no eligible account for model
"claude-sonnet-4-5": af: credential quarantined (sign in to this profile
again); mx: allowance exhausted (resets 2026-08-17T21:00:00Z); ws: credential
unavailable
```

The detail carries account ids, blockers, and reset times only. It never
carries tokens or request content. A quarantined account there means a
sign-in, not a wait: run `rayline subscriptions reload` after signing in
again. If no account has a usable OAuth credential at all, Rayline instead
returns a local 503 that points to `rayline subscriptions status --verbose`;
it does not report credential failure as exhausted allowance.

## Response Headers and Claude UI

Claude Code's built-in `/usage` remains tied to the control profile. Meanwhile,
successful Messages requests may have used another account.

Passing a selected secondary account's unified usage headers back to Claude
Code risks caching those limits under the control identity and making its UI
internally inconsistent. In pool mode, Rayline should:

- consume `anthropic-ratelimit-unified-*` headers into its own state
- remove those unified headers from successful pooled responses
- preserve ordinary response metadata, including request IDs and provider-wide
  rate-limit signals
- preserve the complete final unified rejection when the whole pool is unable
  to serve the request

Rayline should provide the truthful aggregate view:

```text
Subscription ws | Fable 47% | af: weekly exhausted | memex: five-hour exhausted
```

The first interface can be `rayline subscriptions status`, with structured JSON
for statusline integrations. A native statusline can follow after the state
model is stable.

The control account's bootstrap and model catalog remain authoritative for what
Claude Code exposes in its UI. V1 should require both:

- the control profile exposes the requested model
- the selected subscription is entitled to use it

Rayline should not synthesize a model catalog from the union of secondary
accounts.

## Credential Storage

On the inspected macOS installation, Claude Code stores credentials in a
Keychain item derived from the canonical config-directory path and can use a
profile-local `.credentials.json` fallback. This naming and payload format are
private implementation details.

Rayline should isolate those details behind a versioned credential-provider
interface with operations equivalent to:

```text
load(source) -> credential + version
store_if_unchanged(source, version, refreshed_credential)
invalidate(source, reason)
```

Required behavior:

- preserve Keychain/file permissions
- use atomic file replacement and mode `0600`
- serialize refresh per credential source
- never hold state locks across network awaits
- detect concurrent updates made by Claude Code or another Rayline process
- avoid copying credentials into Rayline configuration
- redact tokens and account identifiers from diagnostics
- fail closed when a Claude version or credential schema is unsupported

The observed token endpoint and client metadata are also private. Before
shipping refresh support, verify whether Anthropic offers a supported
credential delegation or refresh contract. If not, gate the compatibility
adapter by tested Claude Code versions and keep a clear re-login path.

## Security, Privacy, and Account Boundaries

- Pool membership is opt-in and explicit.
- Each configured source should show enough local metadata for the user to
  confirm which subscription they are adding, without persisting email
  addresses in routine logs.
- The user should be warned before combining identities from different
  organizations or administrative domains.
- Secondary OAuth tokens are used only by local egress workers talking directly
  to Anthropic.
- Hosted Rayline routes must always strip Claude OAuth, as they do today.
- Logs record account IDs chosen by the user, claim names, normalized
  utilization, and decisions—not raw headers, prompts, bodies, or credentials.
- Configuration directories and pool state use restrictive permissions.
- Launch IDs and local proxy credentials are random, short-lived, and redacted.
- Provider terms and plan rules for pooling subscriptions require explicit
  product/legal review before release.

## Code Boundaries

The likely implementation split follows existing crate responsibilities:

### `rayline-subscriptions` (new shared crate)

- pool configuration schema and validation
- normalized usage and claim model
- model-family normalization
- selection and affinity policy
- retry classification
- credential-provider traits
- secret-safe diagnostic types

### `rayline-cli`

- `subscriptions add/remove/list/status/reload` commands
- `--subscription-pool`
- validation that pooling requires `--via proxy`
- launch ID creation and proxy configuration
- user-facing migration from aliases

### `rayline-daemon`

- account worker lifecycle
- background polling and refresh scheduling
- shared pool state
- credential refresh serialization
- status/metrics exposure

### `rayline-proxy`

- subscription-target hook after existing target selection
- Messages endpoint classification
- per-attempt authorization injection
- response gating before streaming
- failover attempt loop
- unified-header parsing and filtering

### `rayline-local-router`

- preserve the existing `subscription` sentinel
- allow that target to resolve through a named local subscription pool
- do not duplicate selection policy

The proxy currently has the necessary retry boundary: outbound request bytes
exist before `send`, and adaptation/streaming begins afterward. Selection,
claims, and retry policy should remain in the shared crate rather than grow into
the proxy's large request handler.

## Rollout Progress

### Phase 0: compatibility fixtures — partially complete

- Captured and sanitized the observed 2.1.220 usage and global-exhaustion
  response shapes.
- Validated Keychain and file credential providers without printing raw
  credentials.
- A Claude Code version capability table and enforcement gate remain.
- A model-scoped Fable rejection where the global weekly limit is not also
  exhausted remains to be captured.

### Phase 1: registry and monitoring — implemented

- Add explicit pool configuration and CLI management.
- Start all Claude sessions from one control profile.
- Poll every registered account and expose aggregate status.
- Implement model normalization and the pure selector.
- Do not switch live credentials yet.

### Phase 2: request-level routing — implemented

- Add local account egress workers.
- Switch only Messages requests on subscription-target legs.
- Add launch/model affinity.
- Implement exact pre-stream quota failover.
- Consume successful unified headers and preserve final pool-exhaustion errors.

### Phase 3: hardening — partially implemented

- Robust refresh-token rotation and multi-process locking are implemented.
- Expand supported endpoints only from observed need.
- Add optional billing policies.
- Add version health checks and graceful degradation.
- Seek or adopt an upstream-supported multi-account credential contract.

## Test Strategy

Default tests must use local fake services and sanitized fixtures:

- parse all observed usage bucket variants
- retain unknown usage and `limits` fields
- normalize endpoint percentages and response-header fractions
- match Fable model IDs/aliases to `display_name: "Fable"`
- compute the minimum headroom across global and model-scoped claims
- show that a Fable-only exhaustion moves Fable but not Sonnet
- show that global exhaustion removes an account for every model
- keep affinity while the selected account remains healthy
- avoid oscillation near a soft threshold
- switch on the sanitized unified 429 fixture
- do not switch on a generic 429, 529, 5xx, or ambiguous network failure
- refresh once on 401 and quarantine on repeated authentication failure
- replay only before downstream headers or body bytes
- preserve the final real unified 429 when the pool is exhausted
- enforce `included_only` when overage is enabled or in use
- prevent secondary credentials from reaching hosted router requests
- redact tokens from logs, metrics, errors, and process arguments
- serialize refresh and survive concurrent credential updates
- keep launch/model-family affinity under concurrent traffic
- preserve non-pooled behavior byte-for-byte when no pool is configured

An ignored live test can exercise two explicitly supplied test subscriptions:

```bash
CLAUDE_BIN=/path/to/claude \
RAYLINE_TEST_SUBSCRIPTION_POOL=/path/to/test-pool.json \
cargo test -p rayline-proxy --test it_claude_subscription_pool \
  -- --ignored --nocapture
```

The live test must never print or record raw credentials, prompts, account
emails, organization IDs, or unsanitized provider responses.

## Acceptance Criteria

The initial feature is complete when:

- one Claude Code session and daemon use only the control config directory
- two or more explicitly registered credential sources appear in pool status
- Rayline reports global and model-scoped constraints per source
- a pre-stream unified quota rejection transparently moves the request to an
  eligible subscription
- exhausting a model-scoped Fable claim does not unnecessarily move other model
  families
- no request is replayed after downstream streaming begins
- a fully exhausted pool produces Claude Code's normal final limit message
- no secondary OAuth credential reaches hosted Rayline or normal logs
- existing launches without `--subscription-pool` behave exactly as before

## Open Questions and Risks

1. **Supported authentication contract:** Can Anthropic expose a supported way
   for a local tool to hold and refresh several Claude subscription identities?
   The inspected credential and refresh formats are private.
2. **Provider policy:** Are all intended combinations of individual and
   organization subscriptions permitted to be pooled this way?
3. **Fable rejection fixture:** The model-scoped usage bucket and Claude Code
   mapping are visible, but a clean live Fable-only hard rejection still needs
   to be captured and sanitized.
4. **Successful-header filtering:** Verify across supported Claude Code versions
   that removing unified subscription headers does not affect behavior beyond
   its usage cache/UI.
5. **Launch identity compatibility:** Proxy basic authentication is covered by
   CONNECT/TLS integration tests. Confirm it remains present in every supported
   Claude Code transport/version.
6. **Credential concurrency:** Define ownership when Claude Code, Rayline, and
   another alias are simultaneously capable of refreshing the same source.
7. **Unknown claims:** Establish conservative fallback behavior as Anthropic
   adds scopes that Rayline does not yet understand.
8. **Control catalog:** Decide whether a secondary-only model should remain
   unsupported or trigger an explicit control-profile migration workflow.
9. **Endpoint scope:** Confirm through traces whether count-tokens or other
   inference-adjacent endpoints must use the same selected account.
10. **Overage race:** Determine how much safety margin is needed to honor
    `included_only` when polling lags behind concurrent usage.

## References

- [Claude Code setup and authentication](https://docs.anthropic.com/en/docs/claude-code/getting-started)
- [Claude Code corporate proxy configuration](https://docs.anthropic.com/en/docs/claude-code/corporate-proxy)
- [Anthropic API errors](https://platform.claude.com/docs/en/api/errors)
- [Anthropic API rate limits](https://platform.claude.com/docs/en/api/rate-limits)
- [Claude Pro usage limits](https://support.claude.com/en/articles/8325606-what-is-the-pro-plan)
- [Claude Fable 5 plan availability](https://support.claude.com/en/articles/15424964-claude-fable-5-on-your-plan)
- [Using Claude Code with Pro or Max](https://support.claude.com/en/articles/11145838-use-claude-code-with-your-pro-or-max-plan)
- [Claude Code `/usage` command](https://support.claude.com/en/articles/14553413-claude-code-cheatsheet)
