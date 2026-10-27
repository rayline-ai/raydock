#!/usr/bin/env bash
# Demo a routing mode: run `raydock <client> --config <MODE>.json` and watch routing.
#
#   interactive (TTY)        -> records an asciinema cast in a split-pane tmux
#                               session: left pane = the client run, right pane =
#                               `raydock top` (live routing metrics).
#   headless (no TTY, or     -> runs the same command, then prints `raydock top
#   DEMO_HEADLESS=1)            --all` as a plain-text transcript (no asciinema /
#                               tmux, which need a real terminal). For CI / scripted
#                               verification.
#
# Usage:
#   ./visual-test.sh [MODE] [PROMPT]
#     MODE    one of the configs in this dir (default: Rc-Rc), e.g. Rc-Rc Rc-L S-Rcl Rl-Rl L-L S-L ...
#     PROMPT  the prompt sent to the client; overrides the default. The Claude
#             default spawns subagents so subagent routing — may-local
#             (Rcl-Rcl/S-Rcl) and per-class LSR routing (Rl-Rl) — is actually visible.
#             (A plain "say pong" never spawns a subagent.) Codex has no `Task`
#             subagents, so its default is a plain prompt (only `routes.main` runs).
#
# Env:
#   CLIENT         which client to drive: claude (default) or codex. Codex only
#                  exercises `routes.main`; supported for subscription-main (S-*)
#                  and local-main (L-*) modes — see the README's Codex column.
#   CODEX_AUTH     codex auth source: auto (default) | subscription | none. `auto`
#                  uses `--auth subscription` for S-* (subscription-main) modes and
#                  no client auth otherwise. Ignored when CLIENT=claude.
#   RAYLINE_BIN    Raydock binary to use (default: raydock from PATH)
#   DEMO_HEADLESS  set to 1 to force the text path even on a TTY
#   WINDOW_SIZE    asciinema --window-size COLSxROWS (default: 220x50)
#
# Claude is forced through `--via proxy` so `raydock top` has metrics to display;
# Codex is pointed at Rayline directly (no proxy) and routes are visible the same way.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${1:-Rc-Rc}"
RAYLINE_BIN="${RAYLINE_BIN:-raydock}"
WINDOW_SIZE="${WINDOW_SIZE:-220x50}"
CLIENT="${CLIENT:-claude}"
CFG="$HERE/$MODE.json"
CAST="$HERE/$MODE-demo.cast"
SOCKET="rayline-demo-$$"

case "$CLIENT" in
  claude|codex) ;;
  *) echo "error: CLIENT must be 'claude' or 'codex' (got '$CLIENT')" >&2; exit 1 ;;
esac

# Default prompt depends on the client: Claude's spawns subagents (so subagent
# routing is visible); Codex has no subagents, so a plain prompt is enough.
DEFAULT_CLAUDE_PROMPT="Launch TWO subagents with the Task tool, in parallel: an Explore subagent and a general-purpose subagent. Each must reply with exactly the word PONG. After both return, output DONE."
DEFAULT_CODEX_PROMPT="Reply with exactly the word PONG, then on a new line output DONE."
if [ -n "${2:-}" ]; then
  PROMPT="$2"
elif [ "$CLIENT" = codex ]; then
  PROMPT="$DEFAULT_CODEX_PROMPT"
else
  PROMPT="$DEFAULT_CLAUDE_PROMPT"
fi

# Codex auth: `auto` derives from the mode's main provider (the mode name's first
# letter — A = your subscription, so ChatGPT for Codex → `--auth subscription`).
CODEX_SUBSCRIPTION=""   # non-empty => pass `--auth subscription`
if [ "$CLIENT" = codex ]; then
  case "${CODEX_AUTH:-auto}" in
    subscription) CODEX_SUBSCRIPTION=1 ;;
    none) CODEX_SUBSCRIPTION="" ;;
    auto) case "$MODE" in S-*) CODEX_SUBSCRIPTION=1 ;; esac ;;
    *) echo "error: CODEX_AUTH must be auto|subscription|none" >&2; exit 1 ;;
  esac
fi
AUTH_DISPLAY=""
[ -n "$CODEX_SUBSCRIPTION" ] && AUTH_DISPLAY="--auth subscription "

command -v "$RAYLINE_BIN" >/dev/null 2>&1 || { echo "error: '$RAYLINE_BIN' not found on PATH" >&2; exit 1; }
[ -f "$CFG" ] || { echo "error: no config '$CFG' (pick a MODE from this dir)" >&2; exit 1; }
if [ "$CLIENT" = codex ]; then
  command -v codex >/dev/null 2>&1 || { echo "error: 'codex' not found on PATH (needed for CLIENT=codex)" >&2; exit 1; }
fi

# Build the client invocation. `run_client` runs it directly (headless); LEFT_CMD is
# the single-quoted string form for the tmux `send-keys` driver. INVOCATION is a
# human-readable header. Kept in one place so claude/codex don't drift.
if [ "$CLIENT" = codex ]; then
  INVOCATION="$RAYLINE_BIN codex --config $MODE.json ${AUTH_DISPLAY}exec"
  LEFT_CMD="'$RAYLINE_BIN' codex --config '$CFG' ${AUTH_DISPLAY}exec '$PROMPT'"
  run_client() {
    if [ -n "$CODEX_SUBSCRIPTION" ]; then
      "$RAYLINE_BIN" codex --config "$CFG" --auth subscription exec "$PROMPT"
    else
      "$RAYLINE_BIN" codex --config "$CFG" exec "$PROMPT"
    fi
  }
else
  INVOCATION="$RAYLINE_BIN claude --config $MODE.json --via proxy -- -p"
  LEFT_CMD="'$RAYLINE_BIN' claude --config '$CFG' --via proxy -- -p '$PROMPT'"
  run_client() { "$RAYLINE_BIN" claude --config "$CFG" --via proxy -- -p "$PROMPT"; }
fi

# Headless: no TTY (CI / scripted) or DEMO_HEADLESS=1. asciinema/tmux need a real
# terminal, so run the command and print `raydock top` as text instead.
if [ "${DEMO_HEADLESS:-}" = 1 ] || [ ! -t 1 ]; then
  echo "=== $MODE: $INVOCATION  (headless, client=$CLIENT) ==="
  echo "--- prompt: $PROMPT"
  echo
  run_client || true
  echo
  echo "=== routing observed (recent requests, newest last) ==="
  # A one-shot `top` snapshot taken after the run shows no *active* requests, so
  # report the *recent* (completed) ones — each line is one routed turn.
  if command -v python3 >/dev/null 2>&1; then
    "$RAYLINE_BIN" top --json --all 2>/dev/null | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
rows = d.get("recent") or []
if not rows:
    print("  (no recent requests — did the run reach the router?)")
for r in reversed(rows[:24]):
    cls = r.get("agent_type") or "main"
    print("  %-16s target=%-10s model=%s" % (cls, r.get("target"), r.get("selected_model")))
' || "$RAYLINE_BIN" top --all || true
  else
    "$RAYLINE_BIN" top --all || true
  fi
  exit 0
fi

# Interactive: record an asciinema cast driving a tmux split. The tmux driver runs
# *inside* the recording. It builds the split, starts `raydock top` on the right,
# drives the command on the left from a background scheduler, then kills the server
# so the foreground attach (and the recording) ends.
for tool in asciinema tmux; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: '$tool' not found (needed for the recorded demo; set DEMO_HEADLESS=1 for the text path)" >&2; exit 1; }
done
DRIVER="$(mktemp)"
trap 'rm -f "$DRIVER"; tmux -L "$SOCKET" kill-server 2>/dev/null || true' EXIT
cat >"$DRIVER" <<EOF
#!/usr/bin/env bash
set -u
tm() { tmux -L "$SOCKET" "\$@"; }
tm kill-server 2>/dev/null || true
tm new-session -d -s demo -x ${WINDOW_SIZE%x*} -y ${WINDOW_SIZE#*x} -c "$HERE"
tm set -t demo status off
tm split-window -h -t demo:0 -c "$HERE"
tm select-pane -t demo:0.0
tm send-keys -t demo:0.1 "clear; echo '>>> raydock top  (live routing metrics)'; sleep 1; '$RAYLINE_BIN' top --all" Enter
tm send-keys -t demo:0.0 "clear; echo '=== $MODE (client=$CLIENT): $INVOCATION ==='; echo" Enter
(
  sleep 3
  tm send-keys -t demo:0.0 "$LEFT_CMD" Enter
  sleep 45
  tm send-keys -t demo:0.0 "echo; echo '=== demo complete: $MODE routed, observed in raydock top ==='" Enter
  sleep 5
  tm kill-server
) &
exec tmux -L "$SOCKET" attach -t demo
EOF
chmod +x "$DRIVER"

echo "recording $MODE ($CLIENT) -> $CAST"
asciinema rec "$CAST" --overwrite --window-size "$WINDOW_SIZE" -c "$DRIVER"
echo "done. play with: asciinema play '$CAST'"
