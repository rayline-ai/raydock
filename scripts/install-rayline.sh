#!/usr/bin/env sh
set -eu

REPO="rayline-ai/raydock"
VERSION=""
VERSION_GIVEN=false
INSTALL_DIR="$HOME/.rayline/bin"
BASE_URL=""

usage() {
  cat <<'EOF'
Install Rayline CLI.

Usage:
  install-rayline.sh [--version <version>] [--install-dir <path>]

Options:
  --version      Release version to install, for example 0.5.0.
                 Defaults to the latest GitHub release.
  --install-dir  Directory for rayline, rayline-core and rayline-core-LICENSE.
                 Defaults to ~/.rayline/bin.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || {
        echo "missing value for --version" >&2
        exit 2
      }
      case "$2" in
        v*)
          echo "--version takes a bare version such as 0.5.0, not $2" >&2
          exit 2
          ;;
      esac
      VERSION="$2"
      VERSION_GIVEN=true
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
    --base-url)
      # Hidden release-testing option: the directory the release assets are
      # read from.
      [ "$#" -ge 2 ] || {
        echo "missing value for --base-url" >&2
        exit 2
      }
      BASE_URL="$2"
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

if [ -n "$BASE_URL" ] && $VERSION_GIVEN; then
  echo "--base-url and --version cannot be used together" >&2
  exit 2
fi

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "required command not found: $1" >&2
    exit 1
  fi
}

need curl
need awk
if ! command -v minisign >/dev/null 2>&1; then
  echo "required command not found: minisign, which verifies the release signature. Install it: https://jedisct1.github.io/minisign/" >&2
  exit 1
fi

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

if [ -n "$BASE_URL" ]; then
  base_url="${BASE_URL%/}"
elif $VERSION_GIVEN; then
  base_url="https://github.com/$REPO/releases/download/v$VERSION"
else
  base_url="https://github.com/$REPO/releases/latest/download"
fi

rayline_asset="rayline-$platform"
core_asset="rayline-core-$platform"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT INT TERM

download() {
  url="$1"
  dest="$2"
  curl -fsSL "$url" -o "$dest"
}

download "$base_url/SHA256SUMS" "$tmp_dir/SHA256SUMS"
download "$base_url/SHA256SUMS.minisig" "$tmp_dir/SHA256SUMS.minisig"

# rayline production signing public key (matches MINISIGN_PUBLIC_KEYS in rayline-cli; see RELEASING-SIGNING.md).
RAYLINE_PUBKEY="RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1"
if ! minisign -Vm "$tmp_dir/SHA256SUMS" -P "$RAYLINE_PUBKEY" >/dev/null 2>&1; then
  echo "error: SHA256SUMS signature verification failed. The release may be tampered." >&2
  exit 1
fi
echo "SHA256SUMS signature verified."

download "$base_url/$rayline_asset" "$tmp_dir/$rayline_asset"
download "$base_url/$core_asset" "$tmp_dir/$core_asset"
download "$base_url/rayline-core-LICENSE" "$tmp_dir/rayline-core-LICENSE"

for asset in "$rayline_asset" "$core_asset" rayline-core-LICENSE; do
  awk -v asset="$asset" '$1 ~ /^[0-9a-f]+$/ && length($1) == 64 && $0 == $1 "  " asset { print; count++ } END { if (count != 1) exit 1 }' \
    "$tmp_dir/SHA256SUMS" >> "$tmp_dir/SHA256SUMS.selected" || {
      echo "release checksums must include exactly one entry for $asset" >&2
      exit 1
    }
done

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

mkdir -p "$INSTALL_DIR"
install -m 0644 "$tmp_dir/rayline-core-LICENSE" "$INSTALL_DIR/rayline-core-LICENSE"
install -m 0755 "$tmp_dir/$rayline_asset" "$INSTALL_DIR/rayline"
install -m 0755 "$tmp_dir/$core_asset" "$INSTALL_DIR/rayline-core"

echo "Installed Rayline CLI to $INSTALL_DIR"
if ! command -v rayline >/dev/null 2>&1; then
  cat <<EOF

Add Rayline CLI to your PATH:
  export PATH="$INSTALL_DIR:\$PATH"
EOF
fi
