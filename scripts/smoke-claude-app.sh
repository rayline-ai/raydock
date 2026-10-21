#!/usr/bin/env bash
# Wire smoke for `rayline claude app` in a throwaway HOME.
# Starts the Desktop router, runs one headless Claude Code turn with the exact
# model id Desktop sends, asserts the reply, the alias route and the upstream
# ARC model, then stops the router. Never prints the key.
set -euo pipefail
REPO=${REPO:-$(cd "$(dirname "$0")/.." && pwd)}
RAYLINE=${RAYLINE:-$REPO/target/debug/rayline}
export RLD_BIN=${RLD_BIN:-$REPO/target/debug/rld}
CLAUDE=${CLAUDE:-$(command -v claude)}
# Desktop's hidden model ids and the ARC tier each must route to.
# PREFIXED_IDS=1 checks the --prefixed-ids fallback (rayline/arc-*).
if [ -n "${PREFIXED_IDS:-}" ]; then
  MODELS="claude-arc-sage=rayline/arc-sage claude-arc-swift=rayline/arc-swift"; START_FLAGS="--prefixed-ids"
else
  MODELS="claude-arc-sage=arc-sage claude-arc-swift=arc-swift"; START_FLAGS=""
fi
OUT=$(mktemp -d "${TMPDIR:-/tmp}/claude-app-smoke.XXXXXX")
H=$OUT/home
mkdir -p "$H/.config/rayline" "$H/work"
cleanup() {
  env -i PATH="$PATH" HOME="$H" RLD_BIN="$RLD_BIN" "$RAYLINE" claude app stop >/dev/null 2>&1 || true
  rm -f "$H/.config/rayline/credentials.json"
}
trap cleanup EXIT

# Copy only the prod router key into the throwaway HOME.
python3 -I - "$HOME/.config/rayline/credentials.json" "$H/.config/rayline/credentials.json" <<'PY'
import json, os, sys
src = json.load(open(sys.argv[1]))
key = src["router_keys"]["prod"]["apiKey"]
assert key.startswith("rlk-"), "no prod rlk- key"
fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as fh:
    json.dump({"router_keys": {"prod": {"apiKey": key}}}, fh)
PY

run() { env -i PATH="$PATH" HOME="$H" RLD_BIN="$RLD_BIN" TERM=dumb "$@"; }

echo "== rayline claude app (start)"
run "$RAYLINE" claude app $START_FLAGS </dev/null | tee "$OUT/start.txt"
echo "== validate the generated import file"
run "$RAYLINE" claude app validate --desktop-config "$H/.rayline/claude-desktop/claude-desktop-import.json"
if grep -q 'rlk-' "$H/.rayline/claude-desktop/"*.json; then echo "FAIL: key in instance files"; exit 1; fi

for pair in $MODELS; do
  id=${pair%%=*}; tier=${pair#*=}
  echo "== one headless Claude Code turn as Desktop sends it: $id"
  (cd "$H/work" && env -i PATH="$PATH" HOME="$H" TERM=dumb \
    CLAUDE_CONFIG_DIR="$H/.claude" \
    ANTHROPIC_BASE_URL=http://127.0.0.1:20831 \
    ANTHROPIC_API_KEY=rayline-local-placeholder \
    ANTHROPIC_MODEL="$id" \
    ANTHROPIC_DEFAULT_HAIKU_MODEL="$id" \
    "$CLAUDE" -p "Reply with exactly the word PONG." --output-format json </dev/null) > "$OUT/turn-$id.json"
  curl -fsS http://127.0.0.1:20833/v1/router/top/snapshot > "$OUT/snapshot.json"
  python3 -I - "$OUT/turn-$id.json" "$OUT/snapshot.json" "$H/.rayline/claude-desktop/router.json" "$id" "$tier" <<'PY'
import json, sys
turn = json.load(open(sys.argv[1]))
assert not turn.get("is_error"), turn
assert "PONG" in turn.get("result", ""), turn.get("result")
model_id, tier = sys.argv[4], sys.argv[5]
snap = json.load(open(sys.argv[2]))
recent = [r for r in snap.get("recent", []) if r.get("requested_model") == model_id]
assert recent, f"router saw no {model_id} request"
for r in recent:
    assert r["policy"] == f"model:{model_id}", r["policy"]   # explicit alias route
    assert r["endpoint_id"] == "rayline-arc", r["endpoint_id"]
    assert r["status_code"] == 200 and not r.get("error"), r
router = json.load(open(sys.argv[3]))
assert router["routes"]["model_routes"][model_id]["model"] == tier
assert tier in router["endpoints"][0]["models"]
print(model_id, "->", tier, "reply:", turn["result"].strip(), "policy:", recent[0]["policy"],
      "answered as:", recent[0]["selected_model"])
PY
done
echo "== status"
run "$RAYLINE" claude app status || true
echo "== stop"
run "$RAYLINE" claude app stop
echo "SMOKE OK  (artifacts: $OUT)"
