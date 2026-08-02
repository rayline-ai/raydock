# Session-Aware Claude Subscription Placement and Status

Status: **Implemented — runtime observability follow-up remains**

Last updated: 2026-07-31

Related design:
[Multi-Subscription Claude Routing](multi-claude-subscription-routing.md)

## Summary

Rayline should assign each new Claude Code launch to a subscription using both
provider-reported allowance and the sessions Rayline has already placed. The
assignment should remain stable for the launch while it is usable, with
model-family overrides only when the primary subscription lacks capacity or
entitlement for that model.

Before this implementation, the selector was allowance-aware but greedy: a new
`(launch_id, model_family)` affinity always chooses the eligible account with
the most effective headroom. Equal accounts fall back to account ID ordering.
Several launches can therefore select the same account before the next usage
poll reflects their work.

This design replaces that behavior with session-aware weighted
least-connections:

```text
score(account, claim) =
    remaining_headroom(account, claim)
    ----------------------------------
    1 + active_leases(account, claim)

placement_score(account, model) =
    minimum score across every claim applicable to the model
```

Selection still respects hard exhaustion, entitlement, credential health,
model-scoped limits, and the `included_only` billing policy. It is not
request-level round robin.

The same work adds a private per-launch status snapshot. `rld statusline` reads
that snapshot without loading credentials or contacting Anthropic, allowing a
custom Claude Code status script such as `~/.claude/bin/statusline` to show the
subscription actually serving the launch.

## Motivation

### Greedy placement creates a herd

Rayline currently:

1. polls each account at daemon startup
2. normally refreshes usage every five minutes
3. computes effective headroom as the minimum remaining fraction across the
   applicable global and model-scoped claims
4. chooses the eligible account with the greatest effective headroom
5. records affinity only after asynchronously acquiring the account credential

This is safe for failover, but it has two load-balancing problems.

First, accounts with equal allowance use a stable lexical tie-breaker. New
launches therefore prefer the same account until its reported usage changes.

Second, selection and affinity insertion are separated by an asynchronous
credential operation. Concurrent launches can all observe the same state and
choose the same account before any of them records its assignment.

Provider usage is authoritative but intentionally slow-moving. Rayline needs a
small local reservation signal to cover the time between session placement and
the next provider observation.

### The shared config directory is not the serving account

Pool mode deliberately launches every Claude Code process with one shared
`CLAUDE_CONFIG_DIR`. That directory identifies the control profile, not the
credential used for a Messages request.

A status-line badge derived from `CLAUDE_CONFIG_DIR` will therefore always show
the control profile. It cannot distinguish:

- the launch's primary assigned subscription
- a Fable-specific override
- a quota failover
- a stale or unknown allowance snapshot

Claude Code sends `session_id`, model, and other session data to its configured
status-line command on stdin. It may also include `rate_limits` after a
subscriber response, but pooled mode must not treat those fields as the source
of truth: Claude Code's built-in state belongs to the control profile, while
Rayline may have served the request through a secondary account.

See the
[Claude Code status-line input contract](https://code.claude.com/docs/en/statusline).

## Goals

- Distribute new Claude Code launches instead of draining subscriptions in a
  fixed order.
- Consider every global and model-scoped limit applicable to the requested
  model.
- Immediately account for launches placed since the last provider usage poll.
- Keep a launch on one primary account for prompt-cache locality and
  predictability.
- Allow a model-family override without moving unrelated model families.
- Preserve the existing conservative replay boundary.
- Expose the actual assignment to Claude Code status-line scripts.
- Keep status rendering fast, local, credential-free, and best-effort.
- Avoid persisting raw OAuth credentials, proxy credentials, prompts, account
  emails, or profile paths in session status.

## Non-goals

- Request-by-request round robin.
- Predicting the exact token or compute cost of a future Claude session.
- Replacing Anthropic usage observations with locally estimated usage.
- Combining several subscriptions into Claude Code's built-in `/usage` UI.
- Moving control-plane traffic away from the control profile.
- Keeping the same subscription forever across a later `--resume` launch.
- Calling the usage endpoint or opening Keychain items from a status-line
  render.
- Exposing raw Claude session IDs in ordinary Rayline logs.

## Terminology

**Control profile**
: The single `CLAUDE_CONFIG_DIR` used by Claude Code for settings, projects,
  sessions, MCP configuration, and account UI.

**Credential account**
: A registered subscription whose OAuth credential can serve an Anthropic
  Messages request.

**Claude session**
: A Claude Code conversation identified by the `session_id` supplied to the
  status-line command. It may survive process restarts and `--resume`.

**Rayline launch**
: One `rayline claude` process invocation. It has a new opaque `launch_id`,
  including when it resumes an existing Claude session.

**Primary assignment**
: The credential account selected for the launch's first pooled model family.

**Model override**
: A different account used for one model family because the primary assignment
  is not a good candidate for that family.

**Affinity**
: The stable account preference retained for a launch.

**Active lease**
: A short-lived local load reservation used only for new-session placement. An
  affinity may remain after its active lease stops contributing load.

## Invariants

1. Eligibility is evaluated before load balancing.
2. A hard-exhausted claim always makes an account ineligible for that model.
3. A global exhausted claim makes an account ineligible for every model.
4. A model-scoped exhausted claim affects only that model family.
5. Existing affinity wins while it satisfies the migration policy.
6. New-session balancing never causes request-by-request oscillation.
7. Unknown local load can influence placement, but it never overrides an
   authoritative provider rejection.
8. A request is replayed only after an explicit pre-stream quota, entitlement,
   or refresh failure already allowed by the multi-subscription design.
9. Assignment status contains identifiers and normalized allowance only, never
   credentials.
10. Status write or render failure never breaks proxy traffic or Claude Code.

## Placement Policy

### 1. Normalize the requested model

Use the existing `ModelFamily` normalization. The initial assignment happens
on the first pooled `POST /v1/messages`, not before Claude Code starts, because
that is the first point where Rayline reliably knows the requested model.

### 2. Exclude ineligible accounts

Retain the current eligibility rules:

- credential health must be usable
- entitlement must not be known unavailable
- paid extra usage must not already be in use under `included_only`
- no applicable claim may be hard rejected or fully exhausted

Unknown entitlement remains eligible. Unknown usage remains lower-confidence
than a fresh, complete usage snapshot.

### 3. Compute applicable capacity

For model family `m`, the applicable claims are:

- global five-hour
- global weekly
- every subsequently discovered global included-usage claim
- every model-scoped included-usage claim matching `m`

For claim `c`:

```text
remaining_headroom(account, c) =
    1 - clamp(utilization(account, c), 0, 1)
```

The account's raw effective headroom remains:

```text
minimum remaining_headroom across all applicable claims
```

This preserves the existing bottleneck rule. An account with ample Fable
allowance is not usable when its global weekly allowance is exhausted.

### 4. Form the new-session admission tier

`switch_at_percent` is reused as the first implementation's admission
threshold.

- If at least one eligible account is below the threshold for every applicable
  claim, place a new launch only among those accounts.
- If all eligible accounts are above the threshold, keep all of them available
  so a soft threshold does not strand included allowance.
- Existing affinity is evaluated separately from new-session admission.

Admission and migration should become distinct configuration fields after the
balancing behavior has live evidence. Reusing the existing field initially
avoids a registry migration and preserves current defaults.

### 5. Apply active-lease pressure

Every active assignment contributes load to the claims it can consume:

- a launch assigned to an account contributes one lease to its global claims
- a `(launch, model_family)` assignment contributes one lease to matching
  model-scoped claims

For each applicable claim:

```text
claim_score =
    remaining_headroom
    ------------------
    1 + active_claim_leases
```

The account placement score is the most constrained claim:

```text
placement_score = minimum(claim_score)
```

This is a local placement heuristic, not an allowance estimate. It assumes
active launches have comparable expected demand only until provider usage
catches up.

Example:

| Account | Bottleneck headroom | Active applicable leases | Score |
| --- | ---: | ---: | ---: |
| `af` | 80% | 2 | 0.267 |
| `ws` | 50% | 0 | 0.500 |
| `memex` | 20% | 0 | 0.200 |

The next launch selects `ws`, even though `af` has more raw headroom. The
reservation then changes the score seen by the next concurrent launch.

For a Fable request, global lease pressure and Fable lease pressure are scored
independently before taking the minimum. This allows:

- Fable to prefer `ws` because `af` has little Fable allowance
- Sonnet to prefer `af` because its global and Sonnet allowance remain healthy

### 6. Use a launch-specific stable tie-breaker

Do not use account ID ordering for equal scores. Derive a deterministic rank
from:

```text
hash(pool_id, launch_id, model_family, account_id)
```

The rank spreads equal-capacity launches while remaining stable for a retry of
the same selection. It is only a tie-breaker; it does not bypass eligibility or
headroom.

### 7. Reserve atomically

Selection and provisional lease insertion must happen under one placement-state
lock:

```text
prune expired active leases

if a usable affinity exists:
    refresh its last-seen time
    return it

snapshot account allowance
lock placement state
recheck affinity
compute active lease counts
select the highest-scoring candidate
insert a provisional lease
unlock placement state

load or refresh the credential asynchronously

if credential acquisition succeeds:
    commit the assignment
else:
    remove the provisional lease
    mark the account unavailable
    retry selection
```

No mutex is held across `.await`.

The second affinity check closes the race between the initial read and
reservation without holding a lock during account-state snapshots or credential
I/O.

## Affinity and Migration

### Primary assignment

The first successful placement creates:

```text
launch_primary[launch_id] = account_id
```

Subsequent model families prefer the primary account when it is eligible and
passes the migration policy.

### Model override

When the primary account is unsuitable for model family `m`, create:

```text
launch_model_override[(launch_id, m)] = account_id
```

The override is independent of other model families. Exhausting Fable on the
primary account does not move Sonnet.

### Admission is not migration

New-session balancing and live-session migration answer different questions:

- **Admission:** where should new work begin?
- **Migration:** is moving an existing conversation worth losing account-local
  prompt-cache affinity?

The first implementation can retain the existing `switch_at_percent` behavior
for migration while using the same value as the admission threshold. The state
model must keep these decisions separate so later configuration can express:

```text
admit_until_percent
migrate_at_percent
```

An existing assignment moves only when:

- it becomes hard ineligible
- an explicit entitlement or credential failure occurs
- it crosses the configured migration/overage guard and a better eligible
  account exists

If every account is under pressure, retain usable affinity rather than
oscillating.

### Failover

Reactive failover keeps the current safety contract:

- update limit or entitlement state from the explicit response
- clear only the affected launch/model affinity
- exclude the rejected account for the current request
- select and reserve another eligible account
- preserve the final real provider rejection when no account succeeds

Generic rate limits, overloads, ambiguous network failures, and responses that
have started streaming do not cause account failover.

## Lease Lifecycle

### Active versus sticky

An active lease and an affinity have different lifetimes.

- The active lease affects placement pressure while the launch is recently
  issuing requests.
- The affinity remains in the bounded affinity map so an idle launch returns to
  the same account.

The initial active lease TTL is 15 minutes since the last pooled request. An
expired active lease stops affecting new placement but does not remove affinity.
A later request reactivates the lease on the same account if that affinity is
still usable.

The exact TTL is a tuning value, not a security boundary. It should be an
internal runtime option until live data justifies exposing it in the registry.

### Process exit

TTL cleanup is sufficient for the first implementation. A later optimization
may let `rayline claude` send a best-effort local release signal when its Claude
child exits. Failure to release must remain harmless.

### Resume behavior

`claude --resume` preserves Claude's `session_id`, but it is a new Rayline
launch with a new `launch_id`. Treat it as a new admission event so current
allowance determines the subscription.

The status line may correlate the new launch with the resumed Claude session,
but the previous assignment is not a routing requirement.

### Restart behavior

Affinity and active lease state may be lost when the pool daemon restarts. The
existing Claude process retains its proxy launch identity, so its next request
creates a new assignment from fresh pool state.

Persistent routing affinity is unnecessary for correctness. The status
sidecars are informational and must not be used to reconstruct credentials.

## Session Assignment Status

### Requirements

The status interface must:

- identify the primary and currently serving account
- explain a model override or failover
- show compact relevant headroom
- distinguish fresh and stale provider usage
- support several concurrent Claude launches
- avoid network and credential access
- be safe to invoke every status-line render
- degrade to empty output when state is missing

### Why the existing global sidecar is insufficient

The current route-status sidecar is one global file. It is suitable only when
one active launch owns the latest router decision. With several Claude
processes, one session can read another session's most recent status.

Subscription assignment must use a per-launch file. Pool launches should also
write their route status into the same per-launch namespace so
`rld statusline` does not combine a correct subscription assignment with a
route decision from another launch.

Non-pooled launches can retain the current global route-status fallback until
they also gain a launch status identity.

### Status identity

The launcher already creates a random `launch_id` and passes it through local
proxy basic authentication. Do not expose that proxy credential as the status
lookup key.

Instead, the launcher and proxy independently derive:

```text
status_id = hex(sha256("rayline-status-v1\0" + launch_id))
```

The launcher exports only the derived value:

```text
RAYLINE_STATUS_ID=<status_id>
```

The proxy derives the same value after validating local proxy authentication.
The status ID is a lookup identifier, not an Anthropic credential or proxy
password.

### Storage

Pool launch snapshots live under the already private subscription proxy state:

```text
~/.rayline/rld/subscriptions/session-status/<status_id>.json
```

Requirements:

- `session-status/` mode `0700`
- snapshot files mode `0600`
- regular files only; never follow a symlink for a snapshot target
- bounded JSON size
- temp-file plus atomic rename
- best-effort writes
- delete files after 24 hours without an update

The filename contains only the derived status ID. The JSON must not contain the
raw launch ID, proxy URL, OAuth token, profile path, email, prompt, or response
body.

### Snapshot schema

The initial schema is:

```json
{
  "schema": 1,
  "pool_id": "default",
  "assignment": {
    "primary_account_id": "ws",
    "current_account_id": "memex",
    "current_model_family": "fable",
    "kind": "model_override",
    "reason": "quota_failover",
    "assigned_at_unix": 1785490200,
    "last_seen_at_unix": 1785490384
  },
  "capacity": {
    "usage_snapshot_fresh": true,
    "effective_headroom": 0.47,
    "eligible_accounts": 2,
    "total_accounts": 3,
    "bottleneck": {
      "key": "fable_weekly",
      "scope": "model:fable",
      "used_fraction": 0.53,
      "remaining_fraction": 0.47,
      "resets_at": "2026-08-02T10:00:00Z"
    },
    "applicable": [
      {
        "key": "five_hour",
        "scope": "global",
        "used_fraction": 0.28,
        "remaining_fraction": 0.72,
        "resets_at": "2026-07-31T15:00:00Z"
      },
      {
        "key": "seven_day",
        "scope": "global",
        "used_fraction": 0.41,
        "remaining_fraction": 0.59,
        "resets_at": "2026-08-05T09:00:00Z"
      },
      {
        "key": "fable_weekly",
        "scope": "model:fable",
        "used_fraction": 0.53,
        "remaining_fraction": 0.47,
        "resets_at": "2026-08-02T10:00:00Z"
      }
    ]
  },
  "placement": {
    "strategy": "balanced_sessions",
    "score": 0.235,
    "active_global_leases": 2,
    "active_model_leases": 1
  },
  "route": null,
  "updated_at_unix": 1785490384
}
```

When that launch receives a Rayline-routed response, `route` contains the
selected model, virtual model, policy, task class, route ID, and its own update
timestamp. Assignment refreshes preserve the route object, and route refreshes
preserve the subscription assignment.

Fractions use the range `0.0..1.0`. Human renderers must label whether they show
used or remaining allowance. Bare percentages are ambiguous and should not
appear in status output.

`reason` is a stable machine-readable value. Initial values include:

- `balanced_new_launch`
- `primary_affinity`
- `model_override`
- `quota_failover`
- `entitlement_failover`
- `credential_failover`
- `migration_guard`

The snapshot describes only the current account. Aggregate pool status remains
available from `rayline subscriptions status --json`. The two account-count
fields are the model-aware pool reserve: `eligible_accounts` is the number of
configured subscriptions the selector can currently use for this model, while
`total_accounts` is the configured pool size. They deliberately do not sum or
average percentages across plans.

### Write timing

Write or refresh the snapshot:

- when a provisional assignment commits
- when an existing affinity is reused
- after response headers update normalized limit state
- when a model override is created
- when failover changes the current account
- when the route status changes for that launch

Do not write once per streamed response chunk.

## `rld statusline` Contract

### Reader behavior

`rld statusline` remains the fast, dependency-free status-line adapter. It:

1. reads Claude Code's session JSON from stdin
2. reads `RAYLINE_STATUS_ID` from its inherited environment
3. validates that the ID is exactly the expected lowercase hexadecimal shape
4. reads the matching bounded regular snapshot file
5. rejects stale or malformed data
6. renders one compact fragment
7. always exits successfully

It never:

- loads a Claude credential
- reads a Keychain item
- refreshes OAuth
- contacts Anthropic
- polls the pool
- prints an error into the Claude status line

### CLI surface

Extend the command to support:

```text
rld statusline [--component all|route|subscription] [--json]
```

`--component all` remains the default. Missing data produces empty text output.
With `--json`, the command emits one bounded object containing the available
route and subscription fields; missing data produces `{}`.

Examples:

```text
◈ ws · 5h 72%L · 3/3
```

```text
◈ ws→memex · F 47%L · 2/3
```

```text
◈ ws · stale · 3/3
```

`L` means allowance left. The renderer shows only the effective bottleneck:
`5h`, `7d`, or a compact model label such as `F` for Fable. The final fraction
is eligible subscriptions over configured subscriptions for the current model.
Narrow status lines keep the serving account first, and an arrow appears only
when the current model is being served by a subscription other than the
session's primary assignment.

### Custom Claude status-line integration

A custom `~/.claude/bin/statusline` can compose Rayline without reading Rayline
state directly:

```bash
input=$(cat)
rld_bin="${RLD_BIN:-$HOME/.rayline/bin/rld}"

subscription=""
if [ -x "$rld_bin" ]; then
  subscription=$(
    printf '%s' "$input" |
      "$rld_bin" statusline --component subscription 2>/dev/null
  )
fi

if [ -n "$subscription" ]; then
  printf '%s' "$subscription"
else
  # Optional non-pooled fallback based on CLAUDE_CONFIG_DIR.
  printf 'sub:control'
fi
```

For the current local script, this fragment replaces the leading account badge
derived from `CLAUDE_CONFIG_DIR`. The remaining worktree, effort, route, and
session ID segments can stay composed as they are. Skip a separate `cc-usage`
row for pooled launches: it follows the shared control directory, can report the
wrong serving subscription, and may reopen Keychain. Keep it only as a fallback
for non-pooled Claude launches.

The status script should not run:

```bash
rayline subscriptions status
```

That command starts a runtime, loads account credentials, and polls live usage.
It is appropriate for an explicit operator command, not a frequently invoked
render hook.

Claude Code's event-driven status refresh is sufficient after a completed
assistant message. A configured `refreshInterval`, such as 30 seconds, can
update reset countdowns or externally changed state while the main session is
idle.

### Structured consumption

Scripts needing their own colors or layout can use:

```bash
assignment=$(
  printf '%s' "$input" |
    "$rld_bin" statusline --component subscription --json 2>/dev/null
)
account=$(printf '%s' "$assignment" | jq -r '.assignment.current_account_id // empty')
```

The JSON output is the stable integration boundary. Scripts should not depend
on the on-disk path or snapshot filename.

## Data Flow

```text
rayline claude
    |
    | creates launch_id
    | exports derived RAYLINE_STATUS_ID
    | embeds launch_id in loopback proxy authentication
    v
Claude Code --------------------------+
    |                                 |
    | Messages request                | statusLine JSON on stdin
    v                                 v
Rayline proxy                    rld statusline
    |                                 |
    | validates launch_id             | reads RAYLINE_STATUS_ID
    | selects + reserves account      |
    | sends with selected OAuth       |
    | observes allowance headers      |
    | writes private snapshot --------+
    v
Anthropic
```

The status-line path is strictly downstream of selection state. It cannot
change routing.

## Ownership and Crate Boundaries

### `rayline-subscriptions`

- eligibility and claim evaluation
- placement score calculation
- primary and model-override affinity
- active lease lifecycle
- serializable assignment snapshot model
- stable assignment reason values

The pure selector accepts active lease counts as input. It does not perform
filesystem writes.

### `rayline-proxy`

- derive the status ID from validated launch identity
- request the selected assignment from the shared runtime
- update observations and failover reason
- atomically persist the per-launch snapshot
- scope route status to the same launch when available

### `rayline-daemon`

- parse `rld statusline` flags
- safely read the per-launch snapshot
- render compact text or structured JSON
- retain the current global route-status fallback for non-pooled launches

### `rayline-cli`

- create one launch ID
- derive and export `RAYLINE_STATUS_ID`
- keep the shared control `CLAUDE_CONFIG_DIR`
- optionally release the launch lease after child exit in a later phase

Do not duplicate selection or claim logic in CLI handlers or the proxy.

## Observability

### Operator status

Extend pool runtime status with:

- active launch leases per account
- active model-family leases per account
- count of primary assignments
- count of model overrides
- count of recent failovers by reason

`rayline subscriptions status` may show aggregate placement state only when it
queries the already-running pool daemon. Starting a separate status runtime
cannot see the daemon's in-memory leases and must label them unavailable rather
than reporting zero.

This is implemented as a loopback-only daemon control-plane response at
`GET /v1/subscriptions/status`. The CLI probes the subscription instance port
(`20816` by default, or `RAYLINE_SUBSCRIPTION_METRICS_PORT`) with a short
timeout. A successful response contains the daemon's existing allowance
snapshot plus:

- active launch leases per account
- active model-family leases per account
- primary assignment count per account
- model override count per account
- the active-pressure lease TTL

The live query does not initialize a credential store or reopen Keychain. If
the endpoint is absent, belongs to another pool, or cannot be decoded, the CLI
starts the existing standalone allowance poll and emits `placement: null` in
JSON. Recent failover counters remain a follow-up.

### Logs

Local debug logs may contain:

- pool ID
- configured account ID
- normalized model family
- assignment reason
- rounded placement score
- active lease counts

They must not contain:

- raw launch ID
- status ID in full
- Claude session ID
- OAuth access or refresh tokens
- proxy authentication
- account email or organization identity
- prompts or response bodies

### Metrics

Useful counters and gauges:

- `subscription_placement_total{account,reason,model_family}`
- `subscription_active_leases{account,scope}`
- `subscription_failover_total{from_account,to_account,reason,model_family}`
- `subscription_usage_snapshot_age_seconds{account}`

Account IDs are user-configured local labels. Hosted telemetry must not receive
them without an explicit privacy review.

## Security and Privacy

- Assignment status is local-only.
- Snapshot directories and files use private permissions.
- Snapshot reads reject symlinks and oversized input.
- Raw launch identity is never persisted.
- Status IDs are derived with domain separation.
- Status output contains configured account IDs but no profile paths.
- Claude `session_id` is read only for composition and is not required in the
  snapshot.
- Status rendering does not access credential backends.
- A forged local status file can only affect display. It cannot select an
  account or authorize a request.
- The assignment sidecar is not a source of truth after daemon restart.

## Failure Handling

| Failure | Behavior |
| --- | --- |
| Usage snapshot stale | Prefer fresh accounts; retain affinity; label it stale |
| All snapshots unknown | Spread launches with the stable tie-breaker |
| Status env missing | Use route fallback; omit subscription status |
| Status file missing or stale | Emit empty subscription output |
| Status file malformed or oversized | Ignore it and exit successfully |
| Status write fails | Continue proxying; status may be stale |
| Credential acquisition fails | Remove its lease, mark it unavailable, retry |
| Concurrent launch placement | Atomic leases prevent same-state herding |
| Daemon restart | Rebuild placement on next request |
| Every account above threshold | Select the best usable account |
| Explicit model quota rejection | Clear only that launch/model affinity |

## Configuration

The first implementation requires no registry schema change:

- placement strategy becomes the pool default
- `switch_at_percent` supplies the initial admission and migration threshold
- active lease TTL remains an internal runtime option
- accounts have equal local capacity weight

After live validation, additive policy may expose:

```json
{
  "placement": {
    "strategy": "balanced_sessions",
    "admit_until_percent": 90,
    "migrate_at_percent": 90,
    "active_lease_seconds": 900
  }
}
```

Subscriptions with materially different plan capacity may eventually need an
explicit account weight. Rayline should not infer undocumented absolute
capacity from a plan name.

Any registry extension must be versioned or round-trip unknown fields. An older
CLI must not silently erase placement policy when it rewrites the registry.

## Implementation Plan

### Phase 1: Pure balanced selector — implemented

- add applicable per-claim headroom to `AccountEvaluation`
- accept active global and model lease counts
- implement the weighted least-connections score
- replace lexical account tie-breaking with launch-specific stable ranking
- retain all current eligibility and hard-exhaustion behavior

### Phase 2: Atomic leases and two-level affinity — implemented

- replace the affinity map with a placement-state structure
- insert provisional leases before async credential access
- add launch primary and model override mappings
- track last-seen time and prune active pressure after the TTL
- keep bounded sticky affinity independently of active pressure

### Phase 3: Per-launch status — implemented

- derive and export `RAYLINE_STATUS_ID`
- define the serializable assignment snapshot
- write private atomic per-launch status files
- scope pooled route status to the launch snapshot
- clean stale status files

### Phase 4: Status-line reader — implemented

- add component and JSON options to `rld statusline`
- render serving account, override, freshness, and compact headroom
- preserve empty-output and exit-zero behavior
- document composition with existing custom scripts

### Phase 5: Runtime observability and tuning — in progress

- expose lease and assignment counters from the running daemon — implemented
- make `rayline subscriptions status` prefer the live daemon without reopening
  credentials — implemented
- run concurrent launch acceptance tests — implemented with equal-capacity
  synthetic subscriptions
- log account, model family, reason, rounded score, and active lease counts at
  debug level — implemented
- evaluate admission/migration threshold separation
- decide whether account capacity weights are necessary
- consider best-effort child-exit lease release

## Test Plan

### Pure selector

- equal fresh accounts distribute across launch IDs instead of always choosing
  the lexical first ID
- the highest raw headroom does not always win when it already has more active
  leases
- global active leases affect every model
- Fable leases affect Fable without penalizing Sonnet
- the minimum applicable claim remains the bottleneck
- hard exhaustion and credential health still dominate score
- unknown usage is lower priority than complete fresh usage
- all accounts above the admission threshold still yield a selection
- repeated selection for one launch is stable

### Runtime concurrency

- concurrent new launches create distinct provisional reservations
- two first requests for the same launch commit one primary assignment
- credential failure rolls back its provisional lease
- no lock is held across credential refresh
- expired active pressure does not delete sticky affinity
- failover replaces only the affected model override
- the affinity bound evicts least-recently-seen state, not an arbitrary hash-map
  entry

### Status snapshot

- status ID derivation matches between CLI and proxy
- raw launch ID is absent from path, JSON, and logs
- directory and file modes are private
- symlink destinations are rejected
- writes are atomic under concurrent responses
- snapshot size is bounded
- stale cleanup does not touch unrelated files
- model override reports both primary and current account
- fractions are labelled as used or remaining by the renderer

### Status-line reader

- missing env or file produces empty subscription output and exit zero
- malformed, oversized, symlinked, and stale files are ignored
- text output keeps the account when width-sensitive details are absent
- JSON output contains no credential or profile path
- stdin `session_id` remains available to a composing script
- rendering does not initialize a subscription runtime or credential store
- current global route fallback still works outside pool mode

### Integration

- several simultaneous Claude launches spread across equal subscriptions before
  the next usage poll
- a launch retains its primary assignment across ordinary turns
- a Fable override does not move Sonnet
- an explicit hard limit updates assignment status after safe failover
- a second status line reads its own launch snapshot, not another launch's
- Keychain is not reopened by repeated status renders

## Acceptance Criteria

- Ten equal-capacity concurrent synthetic launches do not all select one
  account.
- Placement converges toward remaining capacity while provider usage is fresh.
- No request is moved merely to alternate accounts turn by turn.
- Primary assignment and model override behavior are visible in structured
  status.
- A custom Claude status script can display the serving account with one
  `rld statusline` invocation.
- Repeated status renders perform no network or credential access.
- Repeated aggregate status queries use the running daemon's loaded state and
  do not reopen Keychain when that daemon is reachable.
- Concurrent launches cannot read one another's assignment status.
- Existing hard-limit, entitlement, refresh, and no-replay safety tests remain
  unchanged and passing.

## Open Questions

1. Is a 15-minute active-pressure TTL appropriate for interactive usage, or
   should live acceptance use a shorter value?
2. Should admission and migration thresholds separate immediately, or only
   after balanced placement has field data?
3. Do mixed Pro/Max pools require explicit capacity weights, or does normalized
   provider utilization plus active leases balance well enough?
4. Should the live daemon status endpoint retain recent failover counters in
   memory, or should those remain metrics-only?
5. Should non-pooled Rayline launches also receive a status ID so the existing
   global route sidecar can be retired?
