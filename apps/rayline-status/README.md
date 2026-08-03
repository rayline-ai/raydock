# Rayline Status for macOS

Rayline Status is a native SwiftUI menu-bar app for the default Claude
subscription pool. The menu-bar label shows the number of fully available
accounts, while the popover shows each account's five-hour, seven-day, and
Fable allowance, reset time, projected run-out, availability, and active
launch count.

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
