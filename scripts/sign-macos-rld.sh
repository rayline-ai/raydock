#!/bin/bash
set -euo pipefail

DEFAULT_IDENTIFIER="ai.rayline.rld"

usage() {
    cat <<'EOF'
Sign or verify the macOS rld binary with a stable code-signing identity.

Usage:
  sign-macos-rld.sh [--verify] [path]

The default path is target/release/rld. Signing requires:

  RAYLINE_CODESIGN_IDENTITY   Developer ID or Apple Development identity

Optional environment:

  RAYLINE_CODESIGN_IDENTIFIER          Stable identifier (default: ai.rayline.rld)
  RAYLINE_CODESIGN_REQUIRE_DEVELOPER_ID=1
                                        Reject non-Developer-ID certificates
  RAYLINE_CODESIGN_TIMESTAMP=always|never|auto
                                        Timestamp policy (default: auto)

Ad-hoc signing is deliberately rejected: its designated requirement is the
binary's exact cdhash, so macOS Keychain approval does not survive a rebuild.
EOF
}

verify_only=0
case "${1:-}" in
    --verify)
        verify_only=1
        shift
        ;;
    --help|-h)
        usage
        exit 0
        ;;
esac

if [[ $# -gt 1 ]]; then
    usage >&2
    exit 2
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: macOS code signing requires Darwin" >&2
    exit 1
fi

rld_path="${1:-target/release/rld}"
identifier="${RAYLINE_CODESIGN_IDENTIFIER:-$DEFAULT_IDENTIFIER}"

if [[ ! -f "$rld_path" ]]; then
    echo "error: rld binary not found: $rld_path" >&2
    exit 1
fi

verify_signature() {
    local path="$1"
    local details requirement actual_identifier team

    if ! codesign --verify --strict --verbose=2 "$path"; then
        echo "error: invalid rld code signature: $path" >&2
        return 1
    fi

    details="$(codesign -d --verbose=4 "$path" 2>&1)"
    requirement="$(codesign -d -r- "$path" 2>&1)"
    actual_identifier="$(sed -n 's/^Identifier=//p' <<<"$details" | head -1)"
    team="$(sed -n 's/^TeamIdentifier=//p' <<<"$details" | head -1)"

    if [[ "$actual_identifier" != "$identifier" ]]; then
        echo "error: rld identifier is '$actual_identifier', expected '$identifier'" >&2
        return 1
    fi
    if [[ -z "$team" || "$team" == "not set" ]]; then
        echo "error: rld has no TeamIdentifier (ad-hoc signatures are not stable)" >&2
        return 1
    fi
    if grep -Fq '# designated => cdhash ' <<<"$requirement"; then
        echo "error: rld designated requirement is pinned to one cdhash" >&2
        return 1
    fi
    if [[ "${RAYLINE_CODESIGN_REQUIRE_DEVELOPER_ID:-0}" == "1" ]] \
        && ! grep -Fq 'Authority=Developer ID Application:' <<<"$details"; then
        echo "error: production rld must use a Developer ID Application certificate" >&2
        return 1
    fi

    printf 'Verified rld signature: identifier=%s team=%s\n' "$actual_identifier" "$team"
}

if (( verify_only )); then
    verify_signature "$rld_path"
    exit 0
fi

identity="${RAYLINE_CODESIGN_IDENTITY:-}"
if [[ -z "$identity" ]]; then
    echo "error: set RAYLINE_CODESIGN_IDENTITY to a persistent signing identity" >&2
    echo "available identities:" >&2
    security find-identity -v -p codesigning >&2 || true
    exit 1
fi
if [[ "$identity" == "-" ]]; then
    echo "error: ad-hoc signing cannot preserve Keychain approval" >&2
    exit 1
fi

timestamp_policy="${RAYLINE_CODESIGN_TIMESTAMP:-auto}"
timestamp_args=()
case "$timestamp_policy" in
    always)
        timestamp_args+=(--timestamp)
        ;;
    never)
        ;;
    auto)
        if [[ "$identity" == Developer\ ID\ Application:* ]]; then
            timestamp_args+=(--timestamp)
        fi
        ;;
    *)
        echo "error: RAYLINE_CODESIGN_TIMESTAMP must be always, never, or auto" >&2
        exit 2
        ;;
esac

codesign \
    --force \
    --sign "$identity" \
    --identifier "$identifier" \
    --options runtime \
    "${timestamp_args[@]}" \
    "$rld_path"

verify_signature "$rld_path"
