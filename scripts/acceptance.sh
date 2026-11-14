#!/usr/bin/env bash
# Acceptance matrix: real turns per harness x mode x model x case.
#   modes:  cloud (`rayline run <harness>`), local (`rayline run <harness> --local`)
#   models: one id per provider the harness should drive (default: a Claude,
#           a GPT and an open model); the local process may route elsewhere,
#           so local cells also report the model that executed
#   cases:  plain  one turn, a fixed answer
#           tool   the model must read a file and add, with reasoning on
#           heavy  an MCP tool set whose schemas carry top-level oneOf/anyOf/
#                  allOf (scripts/acceptance/mcp_tools.py); the model must call
#                  one of those tools
#           multi  two turns in one session: the second answer needs what the
#                  first turn read and reasoned about
# Usage: [MODES="cloud local"] [MODELS="<id> ..."] [CASES="plain tool heavy multi"]
#        [CONFIG=<file>] [TIMEOUT=<secs>] [WORK=<dir>] scripts/acceptance.sh [HARNESS...]
#   (default: every harness; a harness that isn't installed fails)
# Local mode uses ~/.config/rayline/settings.json, or <file> when CONFIG is
# set; either must have a `routing` section. With CONFIG, local mode runs
# `rayline local start --config <file>` and `rayline run <harness> --config
# <file>`; cloud mode always uses the default file. The script writes no
# config outside WORK (a fresh temporary directory by default). If a local
# process is already running, or `rayline local status` fails, it exits 1
# before any turn.
# It starts the local process with `rayline local start` and runs
# `rayline local stop` once on exit, including on Ctrl-C, TERM and HUP. If
# `rayline local start` fails, it stops nothing: a process it didn't start is
# left alone.
# A cell a harness cannot run (no per-run MCP config, no model flag) or that
# `rayline run` refuses by design (a resumed session in local mode) is
# reported as SKIP with the reason and does not count as a failure.
# Exit: 0 when no cell failed, 1 when one did, 2 on a usage or config error.
# See docs/acceptance-testing.md.
set -uo pipefail

RAYLINE=${RAYLINE:-rayline}
CONFIG=${CONFIG:-}
TIMEOUT=${TIMEOUT:-600}
MODES=${MODES:-"cloud local"}
MODELS=${MODELS:-"anthropic/claude-sonnet-5.5 openai/gpt-6.1-sol z-ai/glm-5.3-flash"}
CASES=${CASES:-"plain tool heavy multi"}
HARNESSES=(claude codex opencode pi omp hermes openclaw)
ALL_MODES=(cloud local)
ALL_CASES=(plain tool heavy multi)
MCP_SERVER=$(cd "$(dirname "$0")" && pwd)/acceptance/mcp_tools.py
RUN_TOKEN=$RANDOM$RANDOM

usage() {
  echo "usage: [MODES=...] [MODELS=...] [CASES=...] [CONFIG=<file>] $0 [HARNESS...]" >&2
  echo "harnesses: ${HARNESSES[*]}; modes: ${ALL_MODES[*]}; cases: ${ALL_CASES[*]}" >&2
  exit 2
}

one_of() {
  local needle=$1 word
  shift
  for word in "$@"; do
    [ "$needle" = "$word" ] && return 0
  done
  return 1
}

installed() {
  command -v "$1" >/dev/null 2>&1
}

# ---------------------------------------------------------------- harnesses

# harness_cmd <harness> <prompt> [args...]: sets `cmd` to the harness's own
# arguments for one non-interactive turn with [args...] added.
harness_cmd() {
  local harness=$1 prompt=$2
  shift 2
  case $harness in
    # Prompt first: --mcp-config and --allowedTools take several values.
    claude) cmd=(-p "$prompt" "$@") ;;
    codex) cmd=(exec --skip-git-repo-check "$@" "$prompt") ;;
    opencode) cmd=(run "$@" "$prompt") ;;
    pi | omp) cmd=(-p "$@" "$prompt") ;;
    # Not -Q: quiet mode hides the "Session:" line the multi case resumes.
    hermes) cmd=(chat "$@" -q "$prompt") ;;
    openclaw) cmd=(agent "$@" -m "$prompt") ;;
  esac
}

# harness_env <harness>: sets `turn_env` to the environment one turn needs.
harness_env() {
  turn_env=()
  # Claude Code reasons only when given a thinking budget.
  [ "$1" = claude ] && turn_env=(MAX_THINKING_TOKENS=8000)
}

# model_args <harness> <model>: sets `args` to the flags that pick <model> and
# turn its reasoning on. Returns 1 for a harness with no per-run model flag.
model_args() {
  local harness=$1 model=$2
  case $harness in
    claude) args=(--model "$model") ;;
    codex) args=(-m "$model" -c model_reasoning_effort=high) ;;
    opencode) args=(-m "rayline/$model" --thinking) ;;
    pi) args=(--model "rayline/$model" --thinking high) ;;
    omp) return 1 ;; # omp only runs models in its catalog, which Rayline writes
    hermes) args=(-m "$model" --reasoning high) ;;
    openclaw) args=(--model "rayline/$model" --thinking high) ;;
  esac
}

# heavy_args <harness> <dir>: sets `args` to the flags that add the MCP tool
# set for one turn (files go in <dir>). Sets `skip` with the reason instead
# when the harness cannot take a per-run MCP server, or never sends such
# schemas.
heavy_args() {
  local harness=$1 dir=$2
  args=()
  skip=
  case $harness in
    claude) skip="Claude Code skips tools with top-level oneOf/anyOf/allOf before any request" ;;
    codex) args=(-c "mcp_servers.acceptance.command=\"python3\"" -c "mcp_servers.acceptance.args=$(mcp_args_json)") ;;
    opencode) skip="OpenCode exposes MCP tools through its Code Mode execute tool; the schemas never leave the harness" ;;
    pi)
      # Pi's default exposure is codemode, which keeps the schemas inside the
      # harness; direct puts them in the model's tool declarations.
      mkdir -p "$dir/.pi"
      printf '{"mcpServers":{"acceptance":{"command":"python3","args":%s,"exposure":"direct"}}}\n' "$(mcp_args_json)" >"$dir/.pi/mcp.json"
      args=(--approve)
      ;;
    omp | hermes | openclaw) skip="$harness takes MCP servers only from its persistent config" ;;
  esac
}

mcp_args_json() {
  printf '["-I","%s","--value","%s"]' "$MCP_SERVER" "$RUN_TOKEN"
}

# session_args <harness> <turn 1|2> <dir>: sets `args` to the flags that put
# the turn in the cell's session. Turn 2 reads `session_out`, turn 1's output,
# for harnesses that only name a session on exit.
session_args() {
  local harness=$1 turn=$2 dir=$3 id
  case $harness in
    claude)
      # A new id per run: Claude Code refuses --session-id for a session that exists.
      [ "$turn" = 1 ] && python3 -c 'import uuid; print(uuid.uuid4())' >"$dir/session-id"
      id=$(cat "$dir/session-id")
      [ "$turn" = 1 ] && args=(--session-id "$id") || args=(--resume "$id")
      ;;
    codex)
      if [ "$turn" = 1 ]; then
        args=()
      else
        id=$(sed -n -E 's/^session id: ([^ ]+).*/\1/p' <<<"$session_out" | head -n 1)
        [ -n "$id" ] || return 1
        args=(resume "$id")
      fi
      ;;
    opencode) args=(-s "ses_acceptance$RUN_TOKEN$turn_tag") ;;
    pi | openclaw) args=(--session-id "acceptance-$RUN_TOKEN$turn_tag") ;;
    omp) [ "$turn" = 1 ] && args=() || args=(-c) ;;
    hermes)
      if [ "$turn" = 1 ]; then
        args=()
      else
        id=$(sed -n -E 's/^Session:[[:space:]]+([^ ]+).*/\1/p' <<<"$session_out" | head -n 1)
        [ -n "$id" ] || return 1
        args=(--resume "$id")
      fi
      ;;
  esac
}

# write_opencode_config <dir>: OpenCode only runs models its config declares,
# and only takes a project config from the working directory, so every
# OpenCode cell gets one that declares the selected models with thinking on.
# It sets no limits: OpenCode runs without them, and the router and the local
# process enforce their own.
write_opencode_config() {
  python3 - "$1/opencode.json" "${models[@]}" <<'EOF'
import json, sys
path, *models = sys.argv[1:]
config = {
    "$schema": "https://opencode.ai/config.json",
    "provider": {"rayline": {"models": {
        model: {
            "name": model,
            "tool_call": True,
            "reasoning": True,
            "options": {"thinking": {"type": "enabled", "budgetTokens": 4000}},
        }
        for model in models
    }}},
}
with open(path, "w") as f:
    json.dump(config, f, indent=2)
    f.write("\n")
EOF
}

# ----------------------------------------------------------------- running

pass=0
fail=0
skipped=0

# Arrays that may be empty are expanded as ${a[@]+"${a[@]}"}: bash 3.2 (macOS)
# treats "${a[@]}" of an empty array as unbound under set -u.

# with_timeout <secs> <cmd...>: runs cmd in its own process group and kills
# the whole group after <secs> or on Ctrl-C (TERM, then KILL after up to 10s),
# so no harness child outlives the turn.
with_timeout() {
  perl -e '
    use POSIX ":sys_wait_h";
    my $secs = shift;
    my $pid = fork // die "fork: $!\n";
    if (!$pid) { setpgrp(0, 0); exec @ARGV or die "exec: $!\n" }
    setpgrp($pid, $pid);
    sub stop_group {
      my ($code, $message) = @_;
      kill "TERM", -$pid;
      for (1 .. 10) { waitpid($pid, WNOHANG); last unless kill 0, -$pid; sleep 1 }
      kill "KILL", -$pid;
      waitpid($pid, 0);
      print STDERR $message if $message;
      exit $code;
    }
    $SIG{INT} = $SIG{TERM} = sub { stop_group(130) };
    $SIG{ALRM} = sub { stop_group(124, "timed out after ${secs}s\n") };
    alarm $secs;
    waitpid($pid, 0);
    exit($? & 127 ? 128 + ($? & 127) : $? >> 8);
  ' "$@"
}

# run_turn <harness> <cwd> <log> <prompt> [args...]: one harness turn in
# <cwd>; output (router keys redacted) goes to <log> and `out`, exit status to
# `status`. In local mode `executed` collects the models the local process
# ran for this turn.
run_turn() {
  local harness=$1 cwd=$2 log=$3 prompt=$4 offset=0
  shift 4
  harness_cmd "$harness" "$prompt" ${model_flags[@]+"${model_flags[@]}"} "$@"
  harness_env "$harness"
  [ "$mode" = local ] && offset=$(wc -l <"$RUNTIME_LOG")
  out=$(cd "$cwd" && with_timeout "$TIMEOUT" env ${turn_env[@]+"${turn_env[@]}"} \
    "$RAYLINE" run "$harness" ${mode_flags[@]+"${mode_flags[@]}"} -- "${cmd[@]}" </dev/null 2>&1 |
    sed -E 's/rlk-[A-Za-z0-9_-]+/rlk-<redacted>/g')
  status=${PIPESTATUS[0]}
  printf '%s\n' "$out" >"$log"
  if [ "$mode" = local ]; then
    # Only records with an executed_model: sidecalls complete without one.
    executed=$(tail -n +"$((offset + 1))" "$RUNTIME_LOG" | grep -F '"execution completed"' |
      sed -n -E 's/.*"executed_model":"([^"]*)".*/\1/p' | sort -u | paste -sd, -)
  fi
}

# cause <expected>: one line on why the turn failed, from `status` and `out`:
# the last error line, as a harness prints its fatal error after its warnings.
cause() {
  local line
  if [ "$status" -eq 124 ]; then
    echo "timed out after ${TIMEOUT}s"
  elif [ "$status" -eq 0 ]; then
    echo "answer $1 missing from the output"
  elif line=$(grep -i -E 'error|unsupported|refused|not found|denied|invalid|timed out' <<<"$out" | tail -n 1); then
    echo "exit $status: ${line:0:200}"
  else
    echo "exit $status"
  fi
}

record() {
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$harness" "$mode" "$model" "$case" "$1" "${executed:-}" "$2" >>"$RESULTS"
}

report_pass() {
  local detail=
  [ "$mode" = local ] && detail=" (executed ${executed:-none})"
  echo "PASS    $harness $mode $model $case$detail"
  pass=$((pass + 1))
  record PASS ""
  # A turn that passed but logged an error or a 4xx still has a story.
  grep -m 1 -i -E '\berror\b|\b4[0-9][0-9]\b' <<<"$out" | sed 's/^/NOTE    /' | cut -c1-220
}

report_fail() {
  local why=$1 detail=
  [ "$mode" = local ] && detail=" (executed ${executed:-none})"
  echo "FAIL    $harness $mode $model $case$detail: $why"
  tail -n 15 <<<"$out" | sed 's/^/        /'
  fail=$((fail + 1))
  record FAIL "$why"
}

report_skip() {
  echo "SKIP    $harness $mode $model $case: $1"
  skipped=$((skipped + 1))
  record SKIP "$1"
}

# run_cell: one cell for the current `harness`, `mode`, `model`, `case`. The
# harness runs in <cell dir>/work; the turn logs stay in <cell dir>, where the
# harness has no reason to look.
run_cell() {
  local dir=$WORK/$harness-$mode-${model//\//_}-$case cwd a b log
  local -a args
  cwd=$dir/work
  mkdir -p "$cwd"
  executed=
  model_flags=()
  if ! model_args "$harness" "$model"; then
    [ "$model" = default ] || { report_skip "$harness has no model flag"; return; }
  else
    model_flags=(${args[@]+"${args[@]}"})
  fi
  [ "$harness" = opencode ] && write_opencode_config "$cwd"
  a=$((RANDOM % 90 + 10))
  b=$((RANDOM % 90 + 10))
  printf '%s\n%s\n' "$a" "$b" >"$cwd/answer.txt"
  case $case in
    plain)
      run_turn "$harness" "$cwd" "$dir/turn.log" \
        "Reply with the word ACCEPT immediately followed by the sum of 17 and 25, with no space and nothing else."
      check ACCEPT42
      ;;
    tool)
      run_turn "$harness" "$cwd" "$dir/turn.log" \
        "Read the file $cwd/answer.txt. It holds two integers, one per line. Reply with the word ACCEPT immediately followed by their sum, with no space and nothing else."
      check "ACCEPT$((a + b))"
      ;;
    heavy)
      heavy_args "$harness" "$cwd"
      [ -z "$skip" ] || { report_skip "$skip"; return; }
      run_turn "$harness" "$cwd" "$dir/turn.log" \
        "Call the lookup_record tool with id 7 and reply with the word ACCEPT immediately followed by the value it returns, with no space and nothing else." \
        ${args[@]+"${args[@]}"}
      check "ACCEPT$RUN_TOKEN"
      ;;
    multi)
      turn_tag=$RANDOM
      session_args "$harness" 1 "$dir"
      run_turn "$harness" "$cwd" "$dir/turn-1.log" \
        "Read the file $cwd/answer.txt, which holds two integers, one per line, and remember them. Reply with the word OK and nothing else." \
        ${args[@]+"${args[@]}"}
      [ "$status" -eq 0 ] || { multi_fail "turn 1: $(cause OK)"; return; }
      session_out=$out
      session_args "$harness" 2 "$dir" || { report_fail "turn 1 printed no session id"; return; }
      # Hermes refuses model and reasoning flags on resume: the session keeps its own.
      [ "$harness" = hermes ] && model_flags=()
      # Without the file, turn 2 can only get the integers from the session.
      rm "$cwd/answer.txt"
      run_turn "$harness" "$cwd" "$dir/turn-2.log" \
        "Reply with the word ACCEPT immediately followed by the sum of the two integers from the file you read earlier, with no space and nothing else." \
        ${args[@]+"${args[@]}"}
      [ "$status" -eq 0 ] || { multi_fail "$(cause "ACCEPT$((a + b))")"; return; }
      check "ACCEPT$((a + b))"
      ;;
  esac
}

# multi_fail <cause>: a failed multi turn is a SKIP when `rayline run` itself
# refused the session flags, which local mode does by design for some
# harnesses (it says so before the harness starts).
multi_fail() {
  local refusal
  if refusal=$(grep -m 1 -F 'local mode requires a fresh harness session' <<<"$out"); then
    report_skip "$refusal"
  else
    report_fail "$1"
  fi
}

# check <expected>: PASS when the last turn exited 0 and printed <expected>.
check() {
  if [ "$status" -eq 0 ] && grep -q -F "$1" <<<"$out"; then
    report_pass
  else
    report_fail "$(cause "$1")"
  fi
}

# run_mode: every selected cell in the current `mode`.
run_mode() {
  local -a harness_models
  for harness in "${selected[@]}"; do
    if ! installed "$harness"; then
      for model in "${models[@]}"; do
        for case in "${cases[@]}"; do
          executed=
          out=
          status=127
          report_fail "not installed"
        done
      done
      continue
    fi
    harness_models=("${models[@]}")
    model_args "$harness" "${models[0]}" || harness_models=(default)
    for model in "${harness_models[@]}"; do
      for case in "${cases[@]}"; do
        run_cell
      done
    done
  done
}

# stop_local: runs `rayline local stop` the first time it is called; a failed
# stop is a FAIL.
stopped=false
stop_local() {
  $stopped && return
  stopped=true
  if ! "$RAYLINE" local stop; then
    echo "FAIL    rayline local stop"
    fail=$((fail + 1))
  fi
}

# ------------------------------------------------------------------- checks

selected=("$@")
[ ${#selected[@]} -gt 0 ] || selected=("${HARNESSES[@]}")
for harness in "${selected[@]}"; do
  one_of "$harness" "${HARNESSES[@]}" || { echo "unknown harness: $harness" >&2; usage; }
done
read -r -a modes <<<"$MODES"
read -r -a models <<<"$MODELS"
read -r -a cases <<<"$CASES"
[ ${#modes[@]} -gt 0 ] || { echo "MODES is empty" >&2; usage; }
[ ${#models[@]} -gt 0 ] || { echo "MODELS is empty" >&2; usage; }
[ ${#cases[@]} -gt 0 ] || { echo "CASES is empty" >&2; usage; }
for mode in "${modes[@]}"; do
  one_of "$mode" "${ALL_MODES[@]}" || { echo "unknown mode: $mode" >&2; usage; }
done
for case in "${cases[@]}"; do
  one_of "$case" "${ALL_CASES[@]}" || { echo "unknown case: $case" >&2; usage; }
done
installed python3 || { echo "python3 is needed (MCP tool server, session ids)" >&2; exit 2; }
[ -f "$MCP_SERVER" ] || { echo "missing $MCP_SERVER" >&2; exit 2; }

# Turns run in each cell's working directory, so paths given relative to the
# current directory are made absolute first.
case $RAYLINE in
  */*) RAYLINE=$(cd "$(dirname "$RAYLINE")" && pwd)/$(basename "$RAYLINE") || exit 2 ;;
esac
# `rayline run --config <file>` is in local mode only when <file> has a
# `routing` section, so a CONFIG without one would run no local turn.
local_start=("$RAYLINE" local start)
local_run=(--local)
if [ -n "$CONFIG" ]; then
  CONFIG=$(cd "$(dirname "$CONFIG")" && pwd)/$(basename "$CONFIG") || exit 2
  perl -MJSON::PP -e '
    my $file = shift;
    open my $fh, "<", $file or die "CONFIG=$file: $!\n";
    my $config = eval { JSON::PP->new->decode(do { local $/; <$fh> }) };
    ref $config eq "HASH" or die "CONFIG=$file: not a JSON object\n";
    exists $config->{routing} or die "CONFIG=$file has no routing section, which local mode needs\n";
  ' "$CONFIG" || exit 2
  local_start+=(--config "$CONFIG")
  local_run=(--config "$CONFIG")
fi
RUNTIME_LOG=$HOME/.rayline/local/runtime.log

WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK" || exit 2
WORK=$(cd "$WORK" && pwd) || exit 2
RESULTS=$WORK/results.tsv
printf 'harness\tmode\tmodel\tcase\tresult\texecuted\tcause\n' >"$RESULTS"
echo "== work dir: $WORK"

echo "== versions"
"$RAYLINE" --version || exit 1
for harness in "${selected[@]}"; do
  if installed "$harness"; then
    echo "$harness: $("$harness" --version 2>&1 | head -n 1)"
  else
    echo "$harness: not installed"
  fi
done
echo "== models: ${models[*]}"
echo "== cases: ${cases[*]}"

# The cleanup trap below stops the local process, so the script must not run
# while one it didn't start is running, or when it can't tell.
if one_of local "${modes[@]}"; then
  if ! status_out=$("$RAYLINE" local status 2>&1); then
    echo "$status_out" >&2
    echo "rayline local status failed" >&2
    exit 1
  fi
  if grep -q '^Local process: running' <<<"$status_out"; then
    echo "a local process is already running; stop it first" >&2
    exit 1
  fi
fi

# -------------------------------------------------------------------- runs

if one_of cloud "${modes[@]}"; then
  echo "== cloud mode"
  mode=cloud
  mode_flags=()
  run_mode
fi

if one_of local "${modes[@]}"; then
  echo "== local mode"
  # Set before `rayline local start`, so a signal during start still stops it.
  trap stop_local EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  if "${local_start[@]}"; then
    # After a start, status must exit 0 and report the process running.
    status_out=$("$RAYLINE" local status 2>&1)
    status=$?
    echo "$status_out"
    if [ "$status" -ne 0 ] || ! grep -q '^Local process: running' <<<"$status_out"; then
      echo "FAIL    rayline local status (exit $status)"
      fail=$((fail + 1))
    fi
    if [ -f "$RUNTIME_LOG" ]; then
      mode=local
      mode_flags=("${local_run[@]}")
      run_mode
    else
      echo "FAIL    $RUNTIME_LOG missing after rayline local start; local cells need it"
      fail=$((fail + 1))
    fi
    stop_local
  else
    # A refused start (say, a process is already running) must not stop a
    # process this script didn't start.
    trap - EXIT
    stopped=true
    echo "FAIL    rayline local start"
    fail=$((fail + 1))
  fi
fi

echo "== pass=$pass fail=$fail skip=$skipped"
echo "== results: $RESULTS"
[ "$fail" -eq 0 ]
