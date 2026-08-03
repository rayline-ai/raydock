#!/bin/bash
set -euo pipefail

APP_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIGURATION="${CONFIGURATION:-release}"
OUTPUT_DIR="${OUTPUT_DIR:-$APP_ROOT/dist}"
APP_BUNDLE="$OUTPUT_DIR/Rayline Status.app"

swift build --package-path "$APP_ROOT" --configuration "$CONFIGURATION"
BIN_DIR="$(swift build --package-path "$APP_ROOT" --configuration "$CONFIGURATION" --show-bin-path)"
EXECUTABLE="$BIN_DIR/RaylineStatusApp"

if [[ ! -x "$EXECUTABLE" ]]; then
    echo "RaylineStatusApp build product was not found at $EXECUTABLE" >&2
    exit 1
fi

if [[ -d "$APP_BUNDLE" ]]; then
    rm -rf "$APP_BUNDLE"
fi
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"
cp "$EXECUTABLE" "$APP_BUNDLE/Contents/MacOS/RaylineStatusApp"
cp "$APP_ROOT/Resources/Info.plist" "$APP_BUNDLE/Contents/Info.plist"
codesign --force --deep --sign - "$APP_BUNDLE"

echo "$APP_BUNDLE"
