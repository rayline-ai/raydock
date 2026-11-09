#!/usr/bin/env bash
# Build the Linux release binaries against an old glibc baseline so they start
# on Debian 11/12, Ubuntu 20.04+ and RHEL 8+. A plain `cargo build` on a current
# Ubuntu runner links the runner's glibc and fails on older distros with
# "GLIBC_2.38 not found". The output lands in target/release/ like a normal build.
set -euo pipefail

GLIBC_BASELINE="2.28"
ZIGBUILD_VERSION="0.19.8"
ZIG_VERSION="0.13.0"

case "$(uname -m)" in
  x86_64) triple="x86_64-unknown-linux-gnu" ;;
  aarch64 | arm64) triple="aarch64-unknown-linux-gnu" ;;
  *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
esac

python3 -m pip install --user --quiet "cargo-zigbuild==${ZIGBUILD_VERSION}" "ziglang==${ZIG_VERSION}"
export PATH="$HOME/.local/bin:$PATH"
rustup target add "$triple"

cargo zigbuild --release -p rayline-cli -p rayline-daemon --locked \
  --target "${triple}.${GLIBC_BASELINE}"

mkdir -p target/release
for bin in raydock rld; do
  cp "target/${triple}/release/${bin}" "target/release/${bin}"
  # Highest GLIBC_x.y symbol version the binary requires.
  max="$(objdump -T "target/release/${bin}" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
  echo "${bin}: max required GLIBC_${max}"
  if [[ "$(printf '%s\n%s\n' "$max" "$GLIBC_BASELINE" | sort -V | tail -1)" != "$GLIBC_BASELINE" ]]; then
    echo "::error::${bin} requires GLIBC_${max}, above baseline ${GLIBC_BASELINE}" >&2
    exit 1
  fi
done
