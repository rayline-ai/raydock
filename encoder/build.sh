#!/usr/bin/env bash
# Build rayline-arc-encoder (macOS arm64, Metal) from the pinned llama.cpp source.
#
#   encoder/build.sh [--source-dir DIR] [--output-dir DIR] [--jobs N]
#
# release/encoder-source.json pins the llama.cpp repository, the exact commit and the sha256 of every patch in
# encoder/patches/. The script fetches that commit into --source-dir (default encoder/build/llama.cpp) unless it is
# already there, refuses any other commit or local change, verifies and applies the patches, builds a static libllama
# with the Metal shaders embedded (no curl, no OpenSSL), and links the server against it. It writes:
#   <output-dir>/rayline-arc-encoder          the server
#   <output-dir>/rayline-arc-encoder-LICENSE  the licenses of the bundled third-party code
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
pin="$repo/release/encoder-source.json"
src="$here/build/llama.cpp"
out="$here/build/dist"
jobs="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"

while [ $# -gt 0 ]; do
  case "$1" in
    --source-dir) src="$2"; shift 2 ;;
    --output-dir) out="$2"; shift 2 ;;
    --jobs) jobs="$2"; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "rayline-arc-encoder builds only on macOS arm64 (Metal)" >&2
  exit 1
fi
for tool in git cmake c++ shasum python3; do
  command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done

pin_field() { python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$pin" "$1"; }
repository="$(pin_field repository)"
commit="$(pin_field commit)"
min_macos="$(pin_field min_macos)"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "the pinned commit is not a full sha: $commit" >&2; exit 1; }

# Every patch on disk is pinned, and every pinned patch is on disk with its sha256.
patches=()
while IFS=' ' read -r name sha; do
  file="$here/patches/$name"
  [ -f "$file" ] || { echo "pinned patch $name is missing" >&2; exit 1; }
  actual="$(shasum -a 256 "$file" | cut -d' ' -f1)"
  [ "$actual" = "$sha" ] || { echo "patch $name has sha256 $actual, pinned $sha" >&2; exit 1; }
  patches+=("$file")
done < <(python3 -I -c '
import json, sys
for name, sha in sorted(json.load(open(sys.argv[1]))["patches"].items()):
    print(name, sha)' "$pin")
for file in "$here"/patches/*.patch; do
  case " ${patches[*]} " in *" $file "*) ;; *) echo "patch $(basename "$file") is not pinned" >&2; exit 1 ;; esac
done

if [ ! -d "$src/.git" ]; then
  mkdir -p "$src"
  git -C "$src" init -q
  git -C "$src" remote add origin "$repository"
  git -C "$src" fetch -q --depth 1 origin "$commit"
  git -C "$src" -c advice.detachedHead=false checkout -q FETCH_HEAD
fi
head="$(git -C "$src" rev-parse HEAD)"
[ "$head" = "$commit" ] || { echo "$src is at $head, pinned $commit" >&2; exit 1; }

# A clean checkout gets the patches; one that holds exactly the patches (an earlier run) is reused.
if git -C "$src" diff --quiet HEAD && [ -z "$(git -C "$src" status --porcelain --untracked-files=no)" ]; then
  git -C "$src" apply "${patches[@]}"
elif ! git -C "$src" apply --reverse --check "${patches[@]}" 2>/dev/null ||
     [ "$(git -C "$src" diff HEAD --stat | tail -1)" != "$(cd "$src" && git apply --stat "${patches[@]}" | tail -1)" ]; then
  echo "$src has changes other than the pinned patches" >&2
  exit 1
fi

b="$src/build"
cmake -S "$src" -B "$b" -G "$(command -v ninja >/dev/null && echo Ninja || echo 'Unix Makefiles')" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_DEPLOYMENT_TARGET="$min_macos" -DBUILD_SHARED_LIBS=OFF \
  -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
  -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_SERVER=OFF -DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON \
  -DGGML_NATIVE=OFF >/dev/null
cmake --build "$b" --target llama -j "$jobs" >/dev/null

libs=("$b/src/libllama.a" "$b/ggml/src/libggml.a" "$b/ggml/src/ggml-metal/libggml-metal.a"
      "$b/ggml/src/ggml-blas/libggml-blas.a" "$b/ggml/src/libggml-cpu.a" "$b/ggml/src/libggml-base.a")
for lib in "${libs[@]}"; do [ -f "$lib" ] || { echo "missing $lib" >&2; exit 1; }; done
mkdir -p "$out"
c++ -std=c++17 -O2 -mmacosx-version-min="$min_macos" -o "$out/rayline-arc-encoder" \
  "$here/arc_encoder_server.cpp" "$src/vendor/cpp-httplib/httplib.cpp" \
  -I"$src/include" -I"$src/ggml/include" -I"$src/vendor/cpp-httplib" -I"$src/vendor" \
  "${libs[@]}" -framework Foundation -framework Metal -framework MetalKit -framework Accelerate -pthread

{
  echo "rayline-arc-encoder bundles the third-party code below, built from llama.cpp $commit"
  echo "($repository) with the patches in Rayline's encoder/patches/. Rayline's own code in it is"
  echo "licensed under the Apache License 2.0 (see Rayline's LICENSE)."
  for license in LICENSE vendor/cpp-httplib/LICENSE licenses/LICENSE-jsonhpp; do
    printf '\n==> llama.cpp/%s <==\n\n' "$license"
    cat "$src/$license"
  done
} > "$out/rayline-arc-encoder-LICENSE"

echo "$out/rayline-arc-encoder"
