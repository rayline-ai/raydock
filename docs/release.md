# Release Process

Rayline Local releases are binary-first. The Rust workspace is not published to
crates.io yet; all crates are marked `publish = false` until the package split
is intentionally reviewed.

## Release Tags

Create releases from annotated or signed tags named `vX.Y.Z`:

```bash
git tag -a v0.2.0 -m "Rayline 0.2.0"
git push origin v0.2.0
```

Pushing a `v*` tag runs `.github/workflows/release.yml`. The workflow builds
with Rust 1.88 and embeds:

- `RAYLINE_VERSION=<tag without v>`
- `RAYLINE_CHANNEL=prod`

## GitHub Release Assets

The release workflow uploads:

- `rayline-<platform>` and `rld-<platform>` bare binaries;
- `rayline-<version>-<platform>.tar.gz` archives for macOS/Linux;
- `rayline-<version>-windows_x86_64.zip` for Windows;
- `SHA256SUMS` covering every uploaded binary and archive.

macOS `rld` assets are signed before packaging with a Developer ID Application
certificate and the stable identifier `ai.rayline.rld`. The installer and
self-updater reject ad-hoc, unsigned, incorrectly identified, or non-Developer-ID
macOS daemon assets even when their minisign-protected checksum is otherwise
valid. See `RELEASING-SIGNING.md` for the protected environment secrets.

Supported platform tags match the self-updater contract in
`crates/rayline-cli/src/update.rs`:

- `linux_x86_64`
- `linux_aarch64`
- `macosx_10_12_x86_64`
- `macosx_11_0_arm64`
- `windows_x86_64`

The public install scripts download from GitHub Releases and verify the bare
binary assets against `SHA256SUMS`.

## Self-Update Mirror

The built-in `rayline update` command reads from `https://get.rayline.ai` by
default. After the GitHub Release is created, mirror the bare binary assets and
**both** signature files to:

```text
https://get.rayline.ai/cli/v<version>/rayline-<platform>
https://get.rayline.ai/cli/v<version>/rld-<platform>
https://get.rayline.ai/cli/v<version>/SHA256SUMS
https://get.rayline.ai/cli/v<version>/SHA256SUMS.minisig
```

`SHA256SUMS.minisig` is **required**: `rayline update` verifies it against the
pinned public key before trusting any checksum, and fails closed if it is
missing or invalid.

Then update the channel pointer **and its signature** — both are produced and
signed by the release workflow and uploaded as the `latest.txt` /
`latest.txt.minisig` release assets:

```text
https://get.rayline.ai/cli/latest.txt
https://get.rayline.ai/cli/latest.txt.minisig
```

The pointer contains only the public version string (for example `0.2.0`). The
updater fetches `latest.txt.minisig` and verifies it before trusting the
version, so a **forged or hand-edited** pointer — naming an arbitrary or
never-released version — is rejected. Copy the signed `latest.txt` /
`latest.txt.minisig` from the release assets verbatim; do not hand-edit the
pointer or its signature will no longer match.

> **Known limitation (replay / freeze).** Signing proves the pointer is
> *authentic*, not *fresh*. An attacker who can serve content may replay an
> older, validly-signed `latest.txt` to freeze users on a stale head, or pin
> them to an old (still validly-signed) version above their installed one. The
> client refuses to roll *below* the installed version (`target > current`), but
> full anti-rollback needs freshness — a signed timestamp + expiry or a
> highest-seen-version floor — which is tracked as a follow-up, not delivered by
> pointer signing alone.

For non-production channels, copy the same signed pair under the channel name
(`latest-dev.txt` / `latest-dev.txt.minisig` or `latest-main.txt` /
`latest-main.txt.minisig`); the signature is over the version content, not the
filename, so it stays valid.

## Local Validation

Before tagging:

```bash
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 test --workspace --locked -- --test-threads=1
cargo +1.88.0 clippy --workspace --all-targets --locked -- -D warnings
git diff --check
```

After the release workflow completes, validate at least one installer path:

```bash
scripts/install-rayline.sh --version 0.2.0 --install-dir /tmp/rayline-bin
/tmp/rayline-bin/rayline --version
/tmp/rayline-bin/rayline update --check
```

On macOS, also verify the daemon's stable designated requirement:

```bash
scripts/sign-macos-rld.sh --verify /tmp/rayline-bin/rld
codesign -d -r- /tmp/rayline-bin/rld
```

For installer validation against a staged asset directory:

```bash
RAYLINE_RELEASE_BASE_URL=file:///tmp/rayline-release/cli/v0.2.0 \
  scripts/install-rayline.sh --install-dir /tmp/rayline-bin
```

For self-update validation without replacing the active binary, point the update
base at a local or staged mirror:

```bash
RAYLINE_UPDATE_BASE_URL=file:///tmp/rayline-release \
  /tmp/rayline-bin/rayline update --dry-run --version 0.2.0
```
