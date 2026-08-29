# Rayline Status for macOS

Rayline Status is a native SwiftUI menu-bar app for the default Claude
subscription pool. The menu-bar label shows the number of fully available
accounts. The popover shows one row per account, with a column for the
five-hour, seven-day, and Fable allowance.

Each cell reads as two lines: the share of the allowance still left, and a
countdown (`1h 20m`, `3d 5h`). The countdown normally says when that window
resets. When the forecast expects the allowance to empty first, the cell turns
amber, adds a triangle, and the countdown switches to the projected time to
empty, because that is the number that matters. Hover gives both. `OUT` means
the allowance is already spent, so the countdown is the wait until it returns.

The forecast measures real burn. The app keeps recent utilization samples in
`~/Library/Application Support/ai.rayline.status/usage-history.json`, one
series per account, limit, and window instance, and reads the rate off the
trailing 45 minutes for the five-hour limit or 8 hours for the weekly ones.
Below a 10-minute (or 45-minute) span it has nothing honest to say, so it falls
back to assuming even burn since the window opened. The tooltip names which of
the two produced the number.

Hover a row or a cell to swap the header for the account's availability, the
projected run-out, and the absolute UTC reset time.

The app refreshes once per minute by running:

```bash
rayline subscriptions status --pool default --json --live-only
```

`--live-only` is deliberate: the app never opens Claude credential stores and
therefore cannot cause background Keychain prompts. Start Claude through
Rayline so the subscription-pool daemon is available.

## Build and run

Requirements: macOS 13 or newer, Xcode command-line tools, and a current
Rayline CLI installed at `~/.rayline/bin/rayline`, `/opt/homebrew/bin/rayline`,
or `/usr/local/bin/rayline`. For a custom location, set the app preference:

```bash
defaults write ai.rayline.status RaylineExecutablePath "/path/to/rayline"
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
