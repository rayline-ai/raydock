#!/usr/bin/env sh
set -eu

REPO="rayline-ai/rayline"
VERSION="latest"
INSTALL_DIR="${RAYLINE_INSTALL_DIR:-$HOME/.rayline/bin}"

usage() {
  cat <<'EOF'
Install Rayline Local.

Usage:
  install-rayline.sh [--version <version>] [--install-dir <path>]

Options:
  --version      Release version to install, for example 0.2.0 or v0.2.0.
                 Defaults to the latest GitHub release.
  --install-dir  Directory for rayline and rld.
                 Defaults to ~/.rayline/bin.

Environment:
  RAYLINE_RELEASE_BASE_URL  Override the release asset base URL for staging.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || {
        echo "missing value for --version" >&2
        exit 2
      }
      VERSION="$2"
      shift 2
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || {
        echo "missing value for --install-dir" >&2
        exit 2
      }
      INSTALL_DIR="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "required command not found: $1" >&2
    exit 1
  fi
}

verify_macos_rld_signature() {
  binary="$1"
  expected_identifier="ai.rayline.rld"

  need codesign
  if ! codesign --verify --strict --verbose=2 "$binary"; then
    echo "error: rld has an invalid macOS code signature" >&2
    exit 1
  fi

  signature_details="$(codesign -d --verbose=4 "$binary" 2>&1)"
  designated_requirement="$(codesign -d -r- "$binary" 2>&1)"
  actual_identifier="$(printf '%s\n' "$signature_details" | sed -n 's/^Identifier=//p' | head -1)"
  team_identifier="$(printf '%s\n' "$signature_details" | sed -n 's/^TeamIdentifier=//p' | head -1)"

  if [ "$actual_identifier" != "$expected_identifier" ]; then
    echo "error: rld has unexpected macOS signing identifier: $actual_identifier" >&2
    exit 1
  fi
  if [ -z "$team_identifier" ] || [ "$team_identifier" = "not set" ]; then
    echo "error: rld is ad-hoc signed; refusing an identity-unstable install" >&2
    exit 1
  fi
  if printf '%s\n' "$designated_requirement" | grep -Fq '# designated => cdhash '; then
    echo "error: rld signature is pinned to one build hash" >&2
    exit 1
  fi
  if ! printf '%s\n' "$signature_details" | grep -Fq 'Authority=Developer ID Application:'; then
    echo "error: rld is not signed with a Developer ID Application certificate" >&2
    exit 1
  fi
}

need curl
need awk

os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Darwin:arm64|Darwin:aarch64)
    platform="macosx_11_0_arm64"
    ;;
  Darwin:x86_64)
    platform="macosx_10_12_x86_64"
    ;;
  Linux:x86_64|Linux:amd64)
    platform="linux_x86_64"
    ;;
  Linux:arm64|Linux:aarch64)
    platform="linux_aarch64"
    ;;
  *)
    echo "unsupported platform: $os $arch" >&2
    exit 1
    ;;
esac

if [ -n "${RAYLINE_RELEASE_BASE_URL:-}" ]; then
  base_url="${RAYLINE_RELEASE_BASE_URL%/}"
elif [ "$VERSION" = "latest" ]; then
  base_url="https://github.com/$REPO/releases/latest/download"
else
  tag="v${VERSION#v}"
  base_url="https://github.com/$REPO/releases/download/$tag"
fi

rayline_asset="rayline-$platform"
daemon_asset="rld-$platform"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT INT TERM

download() {
  url="$1"
  dest="$2"
  curl -fsSL "$url" -o "$dest"
}

download "$base_url/SHA256SUMS" "$tmp_dir/SHA256SUMS"

# Signature verification (best-effort: required when minisign is available).
if command -v minisign >/dev/null 2>&1; then
  download "$base_url/SHA256SUMS.minisig" "$tmp_dir/SHA256SUMS.minisig"
  # rayline production signing public key (matches MINISIGN_PUBLIC_KEYS in rayline-cli; see RELEASING-SIGNING.md).
  # Override only for testing against a non-production signing key.
  RAYLINE_PUBKEY="${RAYLINE_MINISIGN_PUBKEY:-RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1}"
  if ! minisign -Vm "$tmp_dir/SHA256SUMS" -P "$RAYLINE_PUBKEY" >/dev/null 2>&1; then
    echo "error: SHA256SUMS signature verification failed. The release may be tampered." >&2
    echo "       To override (not recommended), uninstall minisign before running this script." >&2
    exit 1
  fi
  echo "SHA256SUMS signature verified."
else
  printf 'Notice: minisign not found — skipping signature verification.\n' >&2
  printf '        Install minisign for supply-chain protection: https://jedisct1.github.io/minisign/\n' >&2
  printf '        Proceeding over HTTPS (TOFU).\n' >&2
fi

download "$base_url/$rayline_asset" "$tmp_dir/$rayline_asset"
download "$base_url/$daemon_asset" "$tmp_dir/$daemon_asset"

awk -v a="$rayline_asset" -v b="$daemon_asset" '$2 == a || $2 == b { print }' \
  "$tmp_dir/SHA256SUMS" > "$tmp_dir/SHA256SUMS.selected"

if [ ! -s "$tmp_dir/SHA256SUMS.selected" ]; then
  echo "release checksums do not include expected assets for $platform" >&2
  exit 1
fi

(
  cd "$tmp_dir"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c SHA256SUMS.selected
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c SHA256SUMS.selected
  else
    echo "required command not found: sha256sum or shasum" >&2
    exit 1
  fi
)

if [ "$os" = "Darwin" ]; then
  verify_macos_rld_signature "$tmp_dir/$daemon_asset"
fi

mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp_dir/$rayline_asset" "$INSTALL_DIR/rayline"
install -m 0755 "$tmp_dir/$daemon_asset" "$INSTALL_DIR/rld"

echo "Installed Rayline Local to $INSTALL_DIR"
if ! command -v rayline >/dev/null 2>&1; then
  cat <<EOF

Add Rayline to your PATH:
  export PATH="$INSTALL_DIR:\$PATH"
EOF
fi
