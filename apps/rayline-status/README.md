# Rayline Status for macOS

Rayline Status is a native SwiftUI menu-bar app for the default Claude
subscription pool. It answers one question at a glance: is any account
burning its weekly limit too fast?

The menu-bar icon draws one small bar per account, filled with the share of
the weekly allowance left. Bars are monochrome while every account is on pace.
An account projected to run out before its weekly reset turns orange, a spent
account turns red, and a count of those accounts appears next to the icon. With
more than five accounts the icon shows the five most urgent, in pool order.

The compact 420-point popover uses a shared weekly timeline, one 34-point row
per account in stable pool
order. Amber bars run to predicted depletion; muted red runs from depletion to
the next reset. A mint tick marks that reset, with a dashed continuation for
renewed quota whose future burn is unknown. Green bars indicate an allowance
projected to last until reset. Percentages remain beside the account names.
Unknown/stale readings and forecasts still measuring burn are hatched rather
than presented as usable capacity. Hover a row for exact local times and flags.

A shaded column and headline highlight an overlap of all known depleted weekly
intervals, ending at the first reset. Unknown accounts qualify this as a possible
gap, never a confirmed full-pool outage. The horizon covers at least four days
and extends to the latest reset, up to seven days. Day labels follow local
calendar midnights, including daylight-saving changes. No later weekly cycle is
extrapolated. Five-hour and Fable blockers remain attention flags, so this view
forecasts weekly coverage rather than guaranteeing model availability. Saved
resets remain manual `↺N` flags. Each row shows the first attention flag; a
`+N` badge exposes additional flags on hover. The list scrolls past nine accounts.

The forecast measures real burn. The app keeps recent utilization samples in
`~/Library/Application Support/ai.rayline.status/usage-history.json`, one
series per account, limit, and window instance, and reads the rate off the
trailing 45 minutes for the five-hour limit or 8 hours for the weekly ones.
Below a 10-minute (or 45-minute) span it has nothing honest to say, so it falls
back to assuming even burn since the window opened.

The app refreshes once per minute by first running:

```bash
rayline subscriptions status --pool default --json --live-only
```

`--live-only` is deliberate: the app normally avoids opening Claude credential
stores and keeps the normal refresh free of background credential-store reads.
If no subscription-pool daemon is running, the app retries once without
`--live-only` and with `--poll-usage` for ordinary Claude Code sessions. That
explicit fallback sends a read-only usage GET with each account's existing,
unexpired token. It never refreshes OAuth, writes credentials, or starts a
daemon. Missing/expired credentials and failed requests leave that account's
usage unknown; sign in with Claude to restore monitoring. Saved limit reset
discovery still requires the pool daemon. A failed CLI refresh clears the
displayed snapshot so old percentages and forecasts cannot look current.

## Saved limit resets

Discretionary reset offers are separate from the normal five-hour/weekly
renewal. A row shows `↺N` when the account has OAuth-visible resets usable now;
its tooltip carries the inventory summary. Web-only offers are not visible to
OAuth discovery, so check Claude Usage for those.

The daemon reads evaluated promotion data or bounded, authenticated read-only
usage probes. The app itself still only invokes the CLI. Missing/unevaluated
inventory is unknown; partial results and observations older than ten minutes
are stale and cannot claim usable-now resets. Expired or malformed-expiry
offers are excluded. A discovery error does not invalidate quota readings or
trigger credential refresh.

The **Open Claude Usage** link is manual: sign the browser into the intended
account before redeeming. Rayline never consumes a reset automatically.
Redemption is irreversible; normal weekly renewal remains unchanged.
OAuth discovery cannot see every web-only promotion. See
[Claude's explanation](https://support.claude.com/en/articles/17007452-what-is-a-limit-reset)
and [the observed private OAuth interface](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/usage/claude-reset.ts).

## Build and run

Requirements: macOS 13 or newer, Xcode command-line tools, and a current
Rayline CLI installed at `~/.rayline/bin/rayline`, `/opt/homebrew/bin/rayline`,
or `/usr/local/bin/rayline`. For a custom location, set the app preference:

```bash
defaults write ai.rayline.status RaylineExecutablePath "/path/to/rayline"
```

The CLI must support `subscriptions status --poll-usage`. For a separate local
build that is independent of the official CLI installation:

```bash
cargo +1.88.0 build --release -p rayline-cli --locked
mkdir -p "$HOME/.rayline/subscriptions/bin"
install -m 0755 target/release/rayline "$HOME/.rayline/subscriptions/bin/rayline"
defaults write ai.rayline.status RaylineExecutablePath "$HOME/.rayline/subscriptions/bin/rayline"
```

`RAYLINE_BIN` is also honored when the executable is launched directly from an
environment that defines it.

```bash
cd apps/rayline-status
swift test
./scripts/build-app.sh
open "dist/Rayline Status.app"
```

To install it for the current machine:

```bash
ditto "dist/Rayline Status.app" "/Applications/Rayline Status.app"
open "/Applications/Rayline Status.app"
```

The development app is ad-hoc signed. Distribution should use the Rayline
Developer ID identity and normal notarization workflow.
